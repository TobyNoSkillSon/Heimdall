//! Walking JSON Schemas (schema_validation.subschemas, json_objects) and the request-level checks tool and response
//! schemas share (tool_schema._check_depth, _remote_ref, _lookup_tool_reference).
use super::pyjson::J;
use super::ApiError;

/// Keywords whose values map names to schemas.
const SCHEMA_MAP_KEYWORDS: [&str; 5] = ["properties", "patternProperties", "$defs", "definitions", "dependentSchemas"];
/// Keywords whose values are a schema or a list of schemas.
const SUBSCHEMA_KEYWORDS: [&str; 17] = [
    "items",
    "prefixItems",
    "additionalItems",
    "contains",
    "additionalProperties",
    "unevaluatedItems",
    "unevaluatedProperties",
    "propertyNames",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    "contentSchema",
    "extends",
];

/// A schema nests objects and arrays at most this deep.
pub const MAX_SCHEMA_DEPTH: usize = 64;

/// Visits `schema` and, depth first, every schema nested under it, only at schema positions, so const, enum and
/// examples data are never taken for schemas. `visit` may change the node it is given before its children are read,
/// as Python's generator allows; it returns false to stop the walk. Returns false when stopped.
pub(crate) fn walk(schema: &J, visit: &mut dyn FnMut(&J) -> bool) -> bool {
    if !visit(schema) {
        return false;
    }
    children(schema).iter().all(|child| walk(child, visit))
}

/// The schemas directly under `schema`, at schema positions.
fn children(schema: &J) -> Vec<J> {
    let Some(object) = schema.as_obj() else { return Vec::new() };
    let mut children = Vec::new();
    for (key, item) in object.borrow().0.iter() {
        let key = key.as_str();
        if SCHEMA_MAP_KEYWORDS.contains(&key) && item.as_obj().is_some() {
            children.extend(item.as_obj().unwrap().borrow().0.iter().map(|(_, v)| v.clone()));
        } else if key == "dependencies" && item.as_obj().is_some() {
            children.extend(item.as_obj().unwrap().borrow().0.iter().filter(|(_, v)| v.as_arr().is_none()).map(|(_, v)| v.clone()));
        } else if SUBSCHEMA_KEYWORDS.contains(&key) {
            match item.as_arr() {
                Some(list) => children.extend(list.borrow().iter().cloned()),
                None => children.push(item.clone()),
            }
        } else if (key == "type" || key == "disallow") && item.as_arr().is_some() {
            children.extend(item.as_arr().unwrap().borrow().iter().filter(|v| v.as_obj().is_some()).cloned());
        }
    }
    children
}

/// The longest path from `root` through nested schemas and the local references they follow, recursion aside, read
/// without recursion. A validator compiles references by recursing, so a long chain would exhaust its stack.
pub(crate) fn reference_depth(root: &J) -> usize {
    use std::collections::HashMap;
    let next = |node: &J| {
        let mut out = children(node);
        if let Some(target) = node.get("$ref").filter(|r| r.as_str().is_some_and(|r| r.starts_with('#'))).and_then(|_| referenced(node, root)) {
            out.push(target);
        }
        out
    };
    // None while on the path, so a reference back up the path (recursion) is not followed.
    let mut depths: HashMap<super::pyjson::Id, Option<usize>> = HashMap::new();
    let mut stack: Vec<(J, Vec<J>, usize)> = vec![(root.clone(), next(root), 0)];
    depths.insert(root.id(), None);
    while let Some((node, pending, deepest)) = stack.last_mut() {
        if let Some(child) = pending.pop() {
            match depths.get(&child.id()) {
                Some(Some(depth)) => *deepest = (*deepest).max(*depth),
                Some(None) => {}
                None => {
                    depths.insert(child.id(), None);
                    let grandchildren = next(&child);
                    stack.push((child, grandchildren, 0));
                }
            }
            continue;
        }
        let depth = *deepest + 1;
        depths.insert(node.id(), Some(depth));
        stack.pop();
        if let Some((_, _, parent)) = stack.last_mut() {
            *parent = (*parent).max(depth);
        } else {
            return depth;
        }
    }
    0
}

/// Every schema node of `schema`, in walk order.
pub(crate) fn subschemas(schema: &J) -> Vec<J> {
    let mut out = Vec::new();
    walk(schema, &mut |node| {
        out.push(node.clone());
        true
    });
    out
}

/// Every object anywhere in a JSON document, data included.
pub(crate) fn json_objects(value: &J) -> Vec<J> {
    let mut out = Vec::new();
    let mut pending = vec![value.clone()];
    while let Some(value) = pending.pop() {
        match &value {
            J::Obj(o) => {
                pending.extend(o.borrow().0.iter().map(|(_, v)| v.clone()));
                out.push(value.clone());
            }
            J::Arr(a) => pending.extend(a.borrow().iter().cloned()),
            _ => {}
        }
    }
    out
}

/// Refuses `schema`, as `invalid` names it, if it nests deeper than MAX_SCHEMA_DEPTH; read without recursion, before
/// anything recurses.
pub(crate) fn check_depth(schema: &J, invalid: &str) -> Result<(), ApiError> {
    let mut pending = vec![(schema.clone(), 1usize)];
    while let Some((value, depth)) = pending.pop() {
        let children: Vec<J> = match &value {
            J::Obj(o) => o.borrow().0.iter().map(|(_, v)| v.clone()).collect(),
            J::Arr(a) => a.borrow().clone(),
            _ => continue,
        };
        if depth > MAX_SCHEMA_DEPTH {
            return Err(ApiError::bad_request(format!("{invalid}: nested more than {MAX_SCHEMA_DEPTH} levels deep")));
        }
        pending.extend(children.into_iter().map(|c| (c, depth + 1)));
    }
    Ok(())
}

/// The first reference to another document, refusing a `$schema` or a reference that is not a string.
pub(crate) fn remote_ref(schema: &J) -> Result<Option<String>, ApiError> {
    let mut result = Ok(None);
    walk(schema, &mut |node| {
        let Some(object) = node.as_obj() else { return true };
        let object = object.borrow();
        if object.get("$schema").is_some_and(|s| s.as_str().is_none()) {
            result = Err(ApiError::bad_request("$schema must be a string"));
            return false;
        }
        for key in ["$ref", "$dynamicRef", "$recursiveRef"] {
            let Some(reference) = object.get(key) else { continue };
            let Some(reference) = reference.as_str() else {
                result = Err(ApiError::bad_request(format!("{key} must be a string")));
                return false;
            };
            if !reference.starts_with('#') {
                result = Ok(Some(reference.to_string()));
                return false;
            }
        }
        true
    });
    result
}

/// `urllib.parse.unquote`: percent escapes decoded as UTF-8, invalid sequences replaced.
fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() && bytes[i + 1..i + 3].iter().all(u8::is_ascii_hexdigit) {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).expect("ASCII hex");
            out.push(u8::from_str_radix(hex, 16).expect("hex digits"));
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What a local reference names within `root`: a JSON pointer, the root itself, or an `$anchor`.
pub(crate) fn lookup_reference(reference: Option<&J>, root: &J) -> Result<J, ApiError> {
    let reference = match reference.and_then(J::as_str) {
        Some(r) if r.starts_with('#') => r.to_string(),
        _ => return Err(ApiError::bad_request("unsupported tool parameter reference")),
    };
    let unresolved = || ApiError::bad_request(format!("unresolved tool parameter reference: {reference}"));
    let fragment = unquote(&reference[1..]);
    if fragment.is_empty() {
        return Ok(root.clone());
    }
    if let Some(path) = fragment.strip_prefix('/') {
        let mut current = root.clone();
        for part in path.split('/') {
            let part = part.replace("~1", "/").replace("~0", "~");
            let next = match &current {
                J::Arr(list) => {
                    let canonical = part == "0"
                        || (!part.is_empty()
                            && part.bytes().all(|b| b.is_ascii_digit())
                            && !part.starts_with('0'));
                    let list = list.borrow();
                    canonical.then(|| part.parse::<usize>().ok()).flatten().and_then(|i| list.get(i).cloned())
                }
                J::Obj(object) => object.borrow().get(&part).cloned(),
                _ => None,
            };
            current = next.ok_or_else(unresolved)?;
        }
        return Ok(current);
    }
    let mut found = None;
    walk(root, &mut |node| {
        let hit = ["$anchor", "$dynamicAnchor"]
            .iter()
            .any(|key| node.get(key).is_some_and(|v| v.as_str() == Some(fragment.as_str())));
        if hit {
            found = Some(node.clone());
        }
        !hit
    });
    found.ok_or_else(unresolved)
}

/// The schema a local reference of `node` names, or None.
pub(crate) fn referenced(node: &J, root: &J) -> Option<J> {
    lookup_reference(node.get("$ref").as_ref(), root).ok()
}
