//! Redaction and the structure tree: tags must not keep what was removed from the page.
//!
//! Marked content (MCIDs) that applying redactions changed is found by comparing each MCID's
//! operators before and after. Elements owning such content lose their alternate text, actual
//! text and expansion (which may spell out the removed words); references to marked content that
//! is now empty are dropped from the element.

use std::collections::{HashMap, HashSet};

use pdfcraft_cos::{Dict, Document, ObjRef, Object};

/// Each MCID's operators (serialized), and whether anything is still drawn inside it.
fn by_mcid(data: &[u8]) -> HashMap<i64, (Vec<u8>, bool)> {
    let mut out: HashMap<i64, (Vec<u8>, bool)> = HashMap::new();
    let mut stack: Vec<Option<i64>> = Vec::new();
    for op in pdfcraft_content::parse(data).ops {
        match op.op.as_slice() {
            b"BDC" => {
                let id = match op.operands.get(1) {
                    Some(Object::Dict(d)) => d.get(b"MCID").and_then(Object::as_int),
                    _ => None,
                };
                if let Some(id) = id {
                    out.entry(id).or_default();
                }
                stack.push(id);
                continue;
            }
            b"BMC" => {
                stack.push(None);
                continue;
            }
            b"EMC" => {
                stack.pop();
                continue;
            }
            _ => {}
        }
        let draws = match op.op.as_slice() {
            b"Tj" | b"'" | b"\"" => op.operands.last().and_then(Object::as_string).is_some_and(|s| !s.bytes.is_empty()),
            b"TJ" => {
                op.operands.first().and_then(Object::as_array).is_some_and(|a| a.iter().any(|x| x.as_string().is_some_and(|s| !s.bytes.is_empty())))
            }
            b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"S" | b"s" | b"sh" | b"Do" | b"BI" => true,
            _ => false,
        };
        let mut bytes = Vec::new();
        pdfcraft_content::write_op(&op, &mut bytes);
        for id in stack.iter().flatten() {
            let e = out.entry(*id).or_default();
            e.0.extend_from_slice(&bytes);
            e.1 |= draws;
        }
    }
    out
}

/// The MCIDs whose content changed between `before` and `after`, and those left empty.
pub(crate) fn touched(before: &[u8], after: &[u8]) -> (HashSet<i64>, HashSet<i64>) {
    let (b, a) = (by_mcid(before), by_mcid(after));
    let mut changed = HashSet::new();
    let mut empty = HashSet::new();
    for (id, (ops, _)) in &b {
        match a.get(id) {
            Some((new, draws)) => {
                if new != ops {
                    changed.insert(*id);
                }
                if !draws {
                    empty.insert(*id);
                }
            }
            None => {
                changed.insert(*id);
                empty.insert(*id);
            }
        }
    }
    (changed, empty)
}

fn mcid_of(o: &Object, page: Option<ObjRef>, target: ObjRef) -> Option<i64> {
    match o {
        Object::Int(n) if page == Some(target) => Some(*n),
        Object::Dict(m) if m.name(b"Type") == Some(b"MCR") => {
            let pg = m.get(b"Pg").and_then(Object::as_ref).or(page);
            (pg == Some(target)).then(|| m.get(b"MCID").and_then(Object::as_int)).flatten()
        }
        _ => None,
    }
}

/// Clean the elements owning `changed` marked content on `page`; returns how many changed.
pub(crate) fn clean(doc: &mut Document, page: ObjRef, changed: &HashSet<i64>, empty: &HashSet<i64>) -> Result<usize, pdfcraft_cos::CosError> {
    if changed.is_empty() {
        return Ok(0);
    }
    let Some(root) = doc
        .root()
        .and_then(|r| doc.get(r).as_dict().cloned())
        .and_then(|c| c.get(b"StructTreeRoot").map(|s| doc.resolve(s)).and_then(|s| s.as_dict().cloned()))
    else {
        return Ok(0);
    };
    // Indirect elements with the page they inherit.
    let mut todo: Vec<(Object, Option<ObjRef>)> = root.get(b"K").map(|k| vec![(k.clone(), None)]).unwrap_or_default();
    let mut seen = HashSet::new();
    let mut hits: Vec<(ObjRef, Dict, Option<ObjRef>)> = Vec::new();
    while let Some((k, pg)) = todo.pop() {
        match k {
            Object::Array(a) => todo.extend(a.into_iter().map(|x| (x, pg))),
            Object::Ref(r) => {
                if !seen.insert(r) || seen.len() > 2_000_000 {
                    continue;
                }
                let Some(d) = doc.get(r).as_dict().cloned() else { continue };
                if !d.contains(b"S") {
                    continue;
                }
                let pg = d.get(b"Pg").and_then(Object::as_ref).or(pg);
                let kids: Vec<Object> = match d.get(b"K") {
                    Some(Object::Array(a)) => a.clone(),
                    Some(o) => vec![o.clone()],
                    None => Vec::new(),
                };
                if kids.iter().any(|x| mcid_of(x, pg, page).is_some_and(|m| changed.contains(&m))) {
                    hits.push((r, d.clone(), pg));
                }
                todo.extend(kids.into_iter().map(|x| (x, pg)));
            }
            _ => {}
        }
    }
    let mut n = 0;
    for (r, d, pg) in hits {
        let kids = d.get(b"K").cloned();
        doc.update_dict(r, |e| {
            for key in [&b"Alt"[..], b"ActualText", b"E"] {
                e.remove(key);
            }
            // Marked content that is now empty no longer belongs to the element.
            match kids {
                Some(Object::Array(a)) => {
                    let keep: Vec<Object> = a.into_iter().filter(|x| !mcid_of(x, pg, page).is_some_and(|m| empty.contains(&m))).collect();
                    e.set(b"K".to_vec(), Object::Array(keep));
                }
                Some(o) if mcid_of(&o, pg, page).is_some_and(|m| empty.contains(&m)) => {
                    e.remove(b"K");
                }
                _ => {}
            }
        })?;
        n += 1;
    }
    Ok(n)
}

/// The annotation an object reference (`/Type /OBJR`, inline or indirect) points at.
fn objr_target(doc: &Document, o: &Object) -> Option<ObjRef> {
    let d = match o {
        Object::Dict(d) => d.clone(),
        Object::Ref(r) => doc.get(*r).as_dict().cloned()?,
        _ => return None,
    };
    (d.name(b"Type") == Some(b"OBJR")).then(|| d.get(b"Obj").and_then(Object::as_ref)).flatten()
}

/// Drop the structure tree's object references to `removed` annotations and widgets, which
/// otherwise keep them (and their contents, values and appearances) reachable, so the save
/// keeps them. Elements that lose one also lose their alternate text, actual text and
/// expansion, which may describe what was removed. Returns how many elements changed.
pub(crate) fn drop_objects(doc: &mut Document, removed: &HashSet<ObjRef>) -> Result<usize, pdfcraft_cos::CosError> {
    if removed.is_empty() {
        return Ok(0);
    }
    let Some(root) = doc
        .root()
        .and_then(|r| doc.get(r).as_dict().cloned())
        .and_then(|c| c.get(b"StructTreeRoot").map(|s| doc.resolve(s)).and_then(|s| s.as_dict().cloned()))
    else {
        return Ok(0);
    };
    let mut todo: Vec<Object> = root.get(b"K").map(|k| vec![k.clone()]).unwrap_or_default();
    let mut seen = HashSet::new();
    let mut hits: Vec<ObjRef> = Vec::new();
    while let Some(k) = todo.pop() {
        match k {
            Object::Array(a) => todo.extend(a),
            Object::Ref(r) => {
                if !seen.insert(r) || seen.len() > 2_000_000 {
                    continue;
                }
                let Some(d) = doc.get(r).as_dict().cloned() else { continue };
                if !d.contains(b"S") {
                    continue;
                }
                let kids: Vec<Object> = match d.get(b"K") {
                    Some(Object::Array(a)) => a.clone(),
                    Some(o) => vec![o.clone()],
                    None => Vec::new(),
                };
                if kids.iter().any(|x| objr_target(doc, x).is_some_and(|t| removed.contains(&t))) {
                    hits.push(r);
                }
                todo.extend(kids.into_iter().filter(|x| objr_target(doc, x).is_none()));
            }
            _ => {}
        }
    }
    for &r in &hits {
        let kids = doc.get(r).as_dict().and_then(|d| d.get(b"K").cloned());
        let gone = |x: &Object| objr_target(doc, x).is_some_and(|t| removed.contains(&t));
        let keep: Option<Vec<Object>> = match &kids {
            Some(Object::Array(a)) => Some(a.iter().filter(|x| !gone(x)).cloned().collect()),
            _ => None,
        };
        doc.update_dict(r, |e| {
            for key in [&b"Alt"[..], b"ActualText", b"E"] {
                e.remove(key);
            }
            match keep {
                Some(k) => e.set(b"K".to_vec(), Object::Array(k)),
                None => {
                    e.remove(b"K");
                }
            }
        })?;
    }
    Ok(hits.len())
}
