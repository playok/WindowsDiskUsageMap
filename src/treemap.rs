use eframe::egui::{Rect, pos2};

/// Balanced binary treemap; output order matches input, area is proportional to bytes.
pub fn layout(weights: &[u64], rect: Rect) -> Vec<Rect> {
    let mut result = vec![Rect::NOTHING; weights.len()];
    split(weights, rect, &mut result);
    result
}

fn split(weights: &[u64], rect: Rect, result: &mut [Rect]) {
    if weights.is_empty() {
        return;
    }
    if weights.len() == 1 {
        result[0] = rect;
        return;
    }
    let total: f64 = weights.iter().map(|&v| v as f64).sum();
    if total == 0.0 {
        return;
    }
    let mut left = weights[0] as f64;
    let mut pivot = 1;
    while pivot < weights.len() - 1
        && (left + weights[pivot] as f64 - total / 2.0).abs() < (left - total / 2.0).abs()
    {
        left += weights[pivot] as f64;
        pivot += 1;
    }
    let fraction = (left / total) as f32;
    let (a, b) = if rect.width() >= rect.height() {
        let x = rect.left() + rect.width() * fraction;
        (
            Rect::from_min_max(rect.min, pos2(x, rect.bottom())),
            Rect::from_min_max(pos2(x, rect.top()), rect.max),
        )
    } else {
        let y = rect.top() + rect.height() * fraction;
        (
            Rect::from_min_max(rect.min, pos2(rect.right(), y)),
            Rect::from_min_max(pos2(rect.left(), y), rect.max),
        )
    };
    let (ra, rb) = result.split_at_mut(pivot);
    split(&weights[..pivot], a, ra);
    split(&weights[pivot..], b, rb);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn area_and_bounds_are_preserved() {
        let bounds = Rect::from_min_max(pos2(0.0, 0.0), pos2(1000.0, 600.0));
        let weights = [50, 25, 15, 9, 1];
        let rects = layout(&weights, bounds);
        for (i, r) in rects.iter().enumerate() {
            assert!(bounds.contains_rect(*r));
            assert!((r.area() / bounds.area() - weights[i] as f32 / 100.0).abs() < 0.0001);
            for other in &rects[i + 1..] {
                let x = r.intersect(*other);
                assert!(x.width() <= 0.0 || x.height() <= 0.0);
            }
        }
    }
    #[test]
    fn empty_input_is_safe() {
        assert!(layout(&[], Rect::NOTHING).is_empty());
    }
}
