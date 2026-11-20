use lopdf::{Object, ObjectId};
use std::collections::BTreeMap;

// ── /W width-array normalization ─────────────────────────────────────────────

/// Numeric value of a PDF object (Integer or Real), as f64.
fn obj_num(o: &Object) -> Option<f64> {
    match o {
        Object::Integer(i) => Some(*i as f64),
        Object::Real(r) => Some(*r as f64),
        _ => None,
    }
}

/// Rewrite every CIDFont `/W` array into the `cid [w…]` form before
/// extraction. pdf-extract 0.12 mis-parses the other legal forms: the
/// `c_first c_last w` range form reads the *first cid* as the width and its
/// loop skips `c_last`, so single-glyph ranges (`5 5 742`, what krilla and
/// various producers write) contribute nothing and every glyph falls back to
/// `/DW` — often 0 — collapsing all x geometry. Parsing spec-correctly here
/// and re-emitting as array form fixes extraction without forking the crate.
pub(super) fn normalize_cid_widths(doc: &mut lopdf::Document) {
    let ids: Vec<ObjectId> = doc
        .objects
        .iter()
        .filter(|(_, o)| {
            o.as_dict()
                .ok()
                .is_some_and(|d| d.has(b"CIDSystemInfo") && d.get(b"W").is_ok())
        })
        .map(|(id, _)| *id)
        .collect();
    for id in ids {
        let Ok(w_arr) = doc
            .get_object(id)
            .and_then(|o| o.as_dict().and_then(|d| d.get(b"W")))
        else {
            continue;
        };
        let entries = match w_arr {
            Object::Reference(r) => doc
                .get_object(*r)
                .and_then(|o| o.as_array().cloned())
                .unwrap_or_default(),
            Object::Array(a) => a.clone(),
            _ => continue,
        };
        // Parse spec-correctly (PDF 32000-1 Table 120): `c [w…]` or
        // `c_first c_last w`.
        let mut widths: BTreeMap<i64, f64> = BTreeMap::new();
        let mut i = 0;
        while i + 1 < entries.len() {
            let Ok(c) = entries[i].as_i64() else { break };
            if let Ok(wa) = entries[i + 1].as_array() {
                for (j, w) in wa.iter().enumerate() {
                    if let Some(v) = obj_num(w) {
                        widths.insert(c + j as i64, v);
                    }
                }
                i += 2;
            } else {
                let (Ok(last), Some(w)) = (
                    entries[i + 1].as_i64(),
                    entries.get(i + 2).and_then(obj_num),
                ) else {
                    break;
                };
                for g in c..=last {
                    widths.insert(g, w);
                }
                i += 3;
            }
        }
        if widths.is_empty() {
            continue;
        }
        // Re-emit as array-form entries over runs of consecutive cids
        // (≤100 widths per run, splitting long runs).
        let mut new_w: Vec<Object> = Vec::new();
        let mut run: Vec<(i64, f64)> = Vec::new();
        let flush_run = |run: &mut Vec<(i64, f64)>, new_w: &mut Vec<Object>| {
            for chunk in run.chunks(100) {
                new_w.push(Object::Integer(chunk[0].0));
                new_w.push(Object::Array(
                    chunk
                        .iter()
                        .map(|(_, v)| {
                            let rounded = v.round();
                            if (v - rounded).abs() < 1e-6 {
                                Object::Integer(rounded as i64)
                            } else {
                                Object::Real(*v as f32)
                            }
                        })
                        .collect(),
                ));
            }
            run.clear();
        };
        for (&g, &v) in &widths {
            if let Some(last) = run.last() {
                if last.0 + 1 != g || run.len() >= 100 {
                    flush_run(&mut run, &mut new_w);
                }
            }
            run.push((g, v));
        }
        flush_run(&mut run, &mut new_w);
        let _ = doc
            .get_object_mut(id)
            .and_then(|o| o.as_dict_mut())
            .map(|d| d.set(b"W", Object::Array(new_w)));
    }
}
