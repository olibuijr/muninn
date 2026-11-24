pub fn parse_vec(s: &str) -> Option<Vec<f32>> {
    s.trim_matches(|c| c == '[' || c == ']')
        .split(',')
        .map(|x| x.trim().parse().ok())
        .collect()
}

pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let (mut d, mut na, mut nb) = (0f64, 0f64, 0f64);
    for (x, y) in a.iter().zip(b) {
        d += (*x as f64) * (*y as f64);
        na += (*x as f64).powi(2);
        nb += (*y as f64).powi(2);
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        d / (na.sqrt() * nb.sqrt())
    }
}
