//! Disk usage as a treemap: every folder and file is a rectangle whose area
//! matches its size, laid out with the "squarified" algorithm (Bruls,
//! Huizing & van Wijk) so the rectangles stay close to squares.

use crate::{disk, theme, ui};
use egui::{Rect, Sense, pos2, vec2};

/// The map fills the height left on screen, but never less than this.
const MIN_MAP_H: f32 = 220.0;
const GAP: f32 = 2.0;

pub struct Item {
    pub label: String,
    pub size: u64,
    pub color: egui::Color32,
    pub folder: bool,
}

/// What happened to an item of the map this frame.
pub struct Hit {
    pub index: usize,
    pub double: bool,
}

/// Rectangles for `sizes` (largest first works best) filling `rect`, in the
/// same order. Empty sizes get an empty rectangle.
pub fn squarify(sizes: &[u64], rect: Rect) -> Vec<Rect> {
    let total: f64 = sizes.iter().map(|&s| s as f64).sum();
    let mut out = vec![Rect::NOTHING; sizes.len()];
    if total <= 0.0 || rect.width() <= 0.0 || rect.height() <= 0.0 {
        return out;
    }
    let scale = f64::from(rect.width()) * f64::from(rect.height()) / total;
    let areas: Vec<f64> = sizes.iter().map(|&s| s as f64 * scale).collect();
    let order: Vec<usize> = (0..sizes.len()).filter(|&i| areas[i] > 0.0).collect();

    let mut rest = rect;
    let mut i = 0;
    while i < order.len() {
        let short = f64::from(rest.width().min(rest.height()));
        let mut end = i + 1;
        let mut best = worst(&areas, &order[i..end], short);
        while end < order.len() {
            let next = worst(&areas, &order[i..=end], short);
            if next > best {
                break;
            }
            best = next;
            end += 1;
        }
        let row = &order[i..end];
        let sum: f64 = row.iter().map(|&k| areas[k]).sum();
        if rest.width() >= rest.height() {
            // A column along the left side.
            let w = (sum / f64::from(rest.height())) as f32;
            let mut y = rest.top();
            for &k in row {
                let h = (areas[k] / f64::from(w.max(f32::EPSILON))) as f32;
                out[k] = Rect::from_min_size(pos2(rest.left(), y), vec2(w, h));
                y += h;
            }
            rest.min.x += w;
        } else {
            // A row along the top.
            let h = (sum / f64::from(rest.width())) as f32;
            let mut x = rest.left();
            for &k in row {
                let w = (areas[k] / f64::from(h.max(f32::EPSILON))) as f32;
                out[k] = Rect::from_min_size(pos2(x, rest.top()), vec2(w, h));
                x += w;
            }
            rest.min.y += h;
        }
        i = end;
    }
    out
}

/// The worst aspect ratio in a row of `row` items along a side of `short`.
fn worst(areas: &[f64], row: &[usize], short: f64) -> f64 {
    let sum: f64 = row.iter().map(|&k| areas[k]).sum();
    let (s2, w2) = (sum * sum, short * short);
    row.iter().map(|&k| (w2 * areas[k] / s2).max(s2 / (w2 * areas[k]))).fold(0.0, f64::max)
}

/// Draws the map of `items`. Returns the item clicked, if any.
pub fn draw(ui: &mut egui::Ui, items: &[Item]) -> Option<Hit> {
    let size = vec2(ui.available_width() - 8.0, (ui.available_height() - 8.0).max(MIN_MAP_H));
    let (area, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(area, theme::RADIUS, theme::SUBTLE_BG);
    let sizes: Vec<u64> = items.iter().map(|i| i.size).collect();
    let mut hit = None;
    for (index, (item, rect)) in items.iter().zip(squarify(&sizes, area.shrink(GAP))).enumerate() {
        let rect = rect.shrink(GAP / 2.0);
        if rect.width() < 1.0 || rect.height() < 1.0 {
            continue;
        }
        let resp = ui.interact(rect, ui.id().with(("treemap", index)), Sense::click());
        let fill = theme::tint(item.color, if resp.hovered() { 0.38 } else { 0.22 });
        ui.painter().rect_filled(rect, 4.0, fill);
        ui.painter().rect_stroke(rect, 4.0, egui::Stroke::new(1.0_f32, theme::tint(item.color, 0.55)));
        let size = disk::format_size(item.size);
        if rect.width() > 40.0 && rect.height() > 18.0 {
            let max_w = rect.width() - 10.0;
            let (name, _) = ui::elided(ui, &item.label, theme::font_medium(12.5), theme::TEXT_MAIN, max_w);
            ui.painter().galley(rect.min + vec2(5.0, 4.0), name, theme::TEXT_MAIN);
            if rect.height() > 36.0 {
                let (s, _) = ui::elided(ui, &size, theme::font(11.5), theme::TEXT_DIM, max_w);
                ui.painter().galley(rect.min + vec2(5.0, 20.0), s, theme::TEXT_DIM);
            }
        }
        let hint = if item.folder { "Klik om de map te openen" } else { "Klik om te selecteren, dubbelklik om te openen in Verkenner" };
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("{}\n{size}\n{hint}", item.label));
        if resp.double_clicked() {
            hit = Some(Hit { index, double: true });
        } else if resp.clicked() {
            hit = Some(Hit { index, double: false });
        }
    }
    hit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(r: &Rect) -> f32 {
        r.width() * r.height()
    }

    #[test]
    fn rectangles_fill_the_area_in_proportion() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, 400.0));
        let sizes = [500, 250, 120, 80, 30, 20];

        let rects = squarify(&sizes, rect);

        let total: f32 = rects.iter().map(area).sum();
        assert!((total - area(&rect)).abs() < 1.0, "{total}");
        for (r, &s) in rects.iter().zip(&sizes) {
            assert!((area(r) - area(&rect) * s as f32 / 1000.0).abs() < 1.0);
            assert!(rect.expand(0.01).contains_rect(*r), "{r:?}");
        }
    }

    #[test]
    fn rectangles_do_not_overlap_and_stay_squarish() {
        let rect = Rect::from_min_size(pos2(10.0, 20.0), vec2(800.0, 300.0));
        let sizes: Vec<u64> = (1..=12).rev().map(|i| i * 100).collect();

        let rects = squarify(&sizes, rect);

        for (i, a) in rects.iter().enumerate() {
            for b in &rects[i + 1..] {
                let overlap = a.intersect(*b);
                assert!(overlap.width() <= 0.01 || overlap.height() <= 0.01, "{a:?} {b:?}");
            }
            let ratio = (a.width() / a.height()).max(a.height() / a.width());
            assert!(ratio < 4.0, "{a:?}");
        }
    }

    #[test]
    fn single_item_takes_everything_and_empty_sizes_take_nothing() {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 50.0));

        let rects = squarify(&[0, 42, 0], rect);

        assert_eq!(rects[1], rect);
        assert_eq!((rects[0], rects[2]), (Rect::NOTHING, Rect::NOTHING));
        assert!(squarify(&[], rect).is_empty());
    }
}
