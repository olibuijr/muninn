import Foundation

// MiniGolf engine: deterministic maths. CONTRACT (architect). Pure Swift + Foundation, no simd,
// so the whole sim compiles with swiftc on Linux (tools/jadar-test).
//
// Coordinates (see docs/JADAR.md "Coordinates"): the sim lives in a plane. x = metres to the right
// of the tee line, y = metres forward (away from the tee). Height is a separate Double, up positive.
// The render bridge maps sim (x, y, h) to Platform3D world (x, h, -y) in MGRenderSpace.
//
// Determinism rules for every file under Engine/MiniGolf (except Render/):
// - Double only. Per-step maths is + - * / and sqrt. No sin/cos/atan2/pow/exp/log/hypot in the sim:
//   use MGMath.detSin / detCos.
// - Time is an integer tick (240 Hz). Seconds = Double(tick) * dt, never summed.
// - Arrays only. Never iterate a Set or Dictionary. Sort comparators need an index tie-breaker.
// - No Float.random / Double.random / Date in the sim. Seeded MGRandom only.

/// 2-D vector in the sim plane. JSON form is `[x, y]`.
struct MGVec2: Equatable, Hashable, Sendable {
    var x: Double
    var y: Double

    init(_ x: Double, _ y: Double) { self.x = x; self.y = y }
    static let zero = MGVec2(0, 0)

    static func + (a: MGVec2, b: MGVec2) -> MGVec2 { MGVec2(a.x + b.x, a.y + b.y) }
    static func - (a: MGVec2, b: MGVec2) -> MGVec2 { MGVec2(a.x - b.x, a.y - b.y) }
    static prefix func - (a: MGVec2) -> MGVec2 { MGVec2(-a.x, -a.y) }
    static func * (a: MGVec2, s: Double) -> MGVec2 { MGVec2(a.x * s, a.y * s) }
    static func * (s: Double, a: MGVec2) -> MGVec2 { MGVec2(a.x * s, a.y * s) }
    static func / (a: MGVec2, s: Double) -> MGVec2 { MGVec2(a.x / s, a.y / s) }
    static func += (a: inout MGVec2, b: MGVec2) { a = a + b }
    static func -= (a: inout MGVec2, b: MGVec2) { a = a - b }
    static func *= (a: inout MGVec2, s: Double) { a = a * s }

    /// Fixed evaluation order: x*x' first, then y*y'.
    func dot(_ o: MGVec2) -> Double { x * o.x + y * o.y }
    /// z of the 3-D cross product (positive when `o` is counter-clockwise from self).
    func cross(_ o: MGVec2) -> Double { x * o.y - y * o.x }
    var lengthSquared: Double { x * x + y * y }
    var length: Double { (x * x + y * y).squareRoot() }
    /// Unit vector, or zero for a (near) zero vector.
    var normalized: MGVec2 {
        let l = length
        return l > 1e-12 ? MGVec2(x / l, y / l) : .zero
    }
    /// Rotated +90° (counter-clockwise): (-y, x). For a direction "forward", this points left.
    var perp: MGVec2 { MGVec2(-y, x) }
    func distance(to o: MGVec2) -> Double { (self - o).length }

    /// Rounded to the 0.1 mm network grid. Every stroke START position is quantized on every phone,
    /// so a stroke replays from bit-identical input (see docs/JADAR.md "Netcode").
    var quantized: MGVec2 { MGQuantPos(self).value }
}

extension MGVec2: Codable {
    init(from decoder: Decoder) throws {
        var c = try decoder.unkeyedContainer()
        x = try c.decode(Double.self)
        y = try c.decode(Double.self)
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.unkeyedContainer()
        try c.encode(x)
        try c.encode(y)
    }
}

/// A point with height (rails, previews). JSON form is `[x, y, h]`.
struct MGPoint3: Equatable, Hashable, Sendable {
    var x: Double
    var y: Double
    var h: Double
    init(_ x: Double, _ y: Double, _ h: Double) { self.x = x; self.y = y; self.h = h }
    init(_ p: MGVec2, h: Double) { x = p.x; y = p.y; self.h = h }
    var plane: MGVec2 { MGVec2(x, y) }
}

extension MGPoint3: Codable {
    init(from decoder: Decoder) throws {
        var c = try decoder.unkeyedContainer()
        x = try c.decode(Double.self)
        y = try c.decode(Double.self)
        h = try c.decode(Double.self)
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.unkeyedContainer()
        try c.encode(x)
        try c.encode(y)
        try c.encode(h)
    }
}

/// Position on the network grid: Int32 in units of 0.1 mm. JSON form `[x, y]`.
struct MGQuantPos: Equatable, Hashable, Sendable, Codable {
    var x: Int32
    var y: Int32
    init(x: Int32, y: Int32) { self.x = x; self.y = y }
    init(_ p: MGVec2) {
        x = Int32(clamping: Int64((p.x * 10_000).rounded(.toNearestOrEven)))
        y = Int32(clamping: Int64((p.y * 10_000).rounded(.toNearestOrEven)))
    }
    var value: MGVec2 { MGVec2(Double(x) / 10_000, Double(y) / 10_000) }

    init(from decoder: Decoder) throws {
        var c = try decoder.unkeyedContainer()
        x = try c.decode(Int32.self)
        y = try c.decode(Int32.self)
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.unkeyedContainer()
        try c.encode(x)
        try c.encode(y)
    }
}

/// Axis-aligned bounds in the sim plane plus a height range.
struct MGBounds: Equatable, Sendable {
    var min: MGVec2
    var max: MGVec2
    var minH: Double = 0
    var maxH: Double = 0
    static let empty = MGBounds(min: MGVec2(.infinity, .infinity), max: MGVec2(-.infinity, -.infinity), minH: 0, maxH: 0)
    var size: MGVec2 { max - min }
    var center: MGVec2 { (min + max) * 0.5 }
    var isEmpty: Bool { min.x > max.x || min.y > max.y }
    mutating func add(_ p: MGVec2) {
        min = MGVec2(Swift.min(min.x, p.x), Swift.min(min.y, p.y))
        max = MGVec2(Swift.max(max.x, p.x), Swift.max(max.y, p.y))
    }
    func expanded(by d: Double) -> MGBounds {
        MGBounds(min: min - MGVec2(d, d), max: max + MGVec2(d, d), minH: minH, maxH: maxH)
    }
    func contains(_ p: MGVec2) -> Bool { p.x >= min.x && p.x <= max.x && p.y >= min.y && p.y <= max.y }
}

enum MGMath {
    static let pi = 3.141592653589793
    static let twoPi = 6.283185307179586
    static let halfPi = 1.5707963267948966

    /// Deterministic sine: exact range reduction, symmetry fold to [-pi/2, pi/2], Taylor to x^13
    /// (error below 7e-10). Only + - * / and truncatingRemainder, so it is bit-identical everywhere.
    static func detSin(_ v: Double) -> Double {
        var x = v.truncatingRemainder(dividingBy: twoPi)      // (-2pi, 2pi)
        if x > pi { x -= twoPi } else if x < -pi { x += twoPi } // [-pi, pi]
        if x > halfPi { x = pi - x } else if x < -halfPi { x = -pi - x }
        let x2 = x * x
        // Horner form of x - x^3/3! + x^5/5! - ... + x^13/13!
        var t = 1.0 / 6227020800.0
        t = -1.0 / 39916800.0 + x2 * t
        t = 1.0 / 362880.0 + x2 * t
        t = -1.0 / 5040.0 + x2 * t
        t = 1.0 / 120.0 + x2 * t
        t = -1.0 / 6.0 + x2 * t
        t = 1.0 + x2 * t
        return x * t
    }

    static func detCos(_ v: Double) -> Double { detSin(v + halfPi) }

    /// Binary angle (UInt16, full turn = 65536) to radians. 0 = +x (right), counter-clockwise
    /// positive, so 16384 = +y (straight away from the tee).
    static func radians(binaryAngle a: UInt16) -> Double { Double(a) * (twoPi / 65536.0) }

    /// Unit direction of a binary angle, deterministic.
    static func direction(binaryAngle a: UInt16) -> MGVec2 {
        let r = radians(binaryAngle: a)
        return MGVec2(detCos(r), detSin(r))
    }

    /// Nearest binary angle of a direction. Uses atan2: PRESENTATION / INPUT ONLY (aim, bots
    /// choosing a target); never called inside a stroke simulation.
    static func binaryAngle(of d: MGVec2) -> UInt16 {
        var r = atan2(d.y, d.x)
        if r < 0 { r += twoPi }
        let q = Int((r / twoPi * 65536.0).rounded()) & 0xFFFF
        return UInt16(q)
    }

    /// Rotate `v` by `r` radians counter-clockwise (deterministic).
    static func rotate(_ v: MGVec2, by r: Double) -> MGVec2 {
        let c = detCos(r), s = detSin(r)
        return MGVec2(v.x * c - v.y * s, v.x * s + v.y * c)
    }

    static func clamp(_ v: Double, _ a: Double, _ b: Double) -> Double { v < a ? a : (v > b ? b : v) }
    static func lerp(_ a: Double, _ b: Double, _ t: Double) -> Double { a + (b - a) * t }
    /// t*t*(3-2t) on a clamped t.
    static func smooth01(_ t: Double) -> Double { let c = clamp(t, 0, 1); return c * c * (3 - 2 * c) }

    /// Closest point on segment ab to p, and its parameter in [0, 1].
    static func closestOnSegment(_ p: MGVec2, _ a: MGVec2, _ b: MGVec2) -> (point: MGVec2, t: Double) {
        let ab = b - a
        let l2 = ab.lengthSquared
        if l2 < 1e-18 { return (a, 0) }
        let t = clamp((p - a).dot(ab) / l2, 0, 1)
        return (a + ab * t, t)
    }

    /// Even-odd point-in-polygon (polygon given without repeating the first point).
    static func pointInPolygon(_ p: MGVec2, _ poly: [MGVec2]) -> Bool {
        var inside = false
        var j = poly.count - 1
        for i in 0..<poly.count {
            let a = poly[i], b = poly[j]
            if (a.y > p.y) != (b.y > p.y) {
                let xCross = (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x
                if p.x < xCross { inside.toggle() }
            }
            j = i
        }
        return inside
    }

    /// Signed area (positive = counter-clockwise).
    static func signedArea(_ poly: [MGVec2]) -> Double {
        var s = 0.0
        var j = poly.count - 1
        for i in 0..<poly.count { s += poly[j].cross(poly[i]); j = i }
        return s * 0.5
    }
}
