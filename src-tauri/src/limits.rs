use serde::{Deserialize, Serialize};

/// Rolling window of UNPINNED history rows (Windows Clipboard behaviour).
pub const MAX_ITEMS: usize = 100;
/// Hard ceiling for a stored file: the unpinned window plus pinned rows.
pub const MAX_STORED_ITEMS: usize = 200;
/// Per item ceiling. Anything larger is refused and never written to disk.
pub const MAX_ITEM_BYTES: usize = 4 * 1024 * 1024;
/// Rows above this size are "heavy" (expensive to render and serialize).
pub const MAX_HEAVY_ITEM_BYTES: usize = 256 * 1024;
/// At most this many heavy UNPINNED rows are kept.
pub const MAX_HEAVY_ITEMS: usize = 25;
pub const MAX_TABS: usize = 10;
/// Whole-store byte budget. MAX_ITEMS * MAX_ITEM_BYTES would allow ~4 GB, which
/// serialization cannot hold in memory. KEEP IN SYNC with src/limits.js.
pub const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_NAME_LENGTH: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub text: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Tab {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Data {
    pub version: u32,
    pub items: Vec<Entry>,
    pub tabs: Vec<Tab>,
}

impl Default for Data {
    fn default() -> Self {
        Data { version: 2, items: Vec::new(), tabs: Vec::new() }
    }
}

/// UTF-8 byte length (mirror of getByteSize).
pub fn get_byte_size(s: &str) -> usize {
    s.len()
}

/// Truncate to at most `max_bytes` UTF-8 bytes, never splitting a character.
pub fn truncate_by_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// True when the entry is expensive enough to count against the heavy cap.
pub fn is_heavy(text: &str) -> bool {
    text.len() > MAX_HEAVY_ITEM_BYTES
}

/// Rolling-window retention. Pinned rows are never evicted; the list is
/// newest-first, so anything dropped is the oldest unpinned row of its class:
///   - at most MAX_ITEMS unpinned rows
///   - at most MAX_HEAVY_ITEMS unpinned rows above MAX_HEAVY_ITEM_BYTES
pub fn apply_retention(items: Vec<Entry>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::with_capacity(items.len().min(MAX_STORED_ITEMS));
    let mut unpinned = 0usize;
    let mut heavy = 0usize;
    for entry in items {
        if entry.pinned {
            out.push(entry);
            continue;
        }
        if unpinned >= MAX_ITEMS {
            continue;
        }
        let hv = is_heavy(&entry.text);
        if hv && heavy >= MAX_HEAVY_ITEMS {
            continue;
        }
        unpinned += 1;
        if hv {
            heavy += 1;
        }
        out.push(entry);
    }
    out
}

fn normalize_uid(uid: &Option<String>) -> Option<String> {
    match uid {
        Some(u) if !u.is_empty() && u.chars().count() <= MAX_NAME_LENGTH => Some(u.clone()),
        _ => None,
    }
}


pub fn normalize_tab(tab: &Tab) -> Option<Tab> {
    if tab.id.is_empty() || tab.id.chars().count() > MAX_NAME_LENGTH {
        return None;
    }
    if tab.name.is_empty() || tab.name.chars().count() > MAX_NAME_LENGTH {
        return None;
    }
    if tab.ids.len() > MAX_STORED_ITEMS {
        return None;
    }
    let ids = tab
        .ids
        .iter()
        .filter(|id| id.chars().count() <= MAX_NAME_LENGTH)
        .cloned()
        .collect();
    Some(Tab { id: tab.id.clone(), name: tab.name.clone(), ids })
}

/// Strict validation — mirrors validateData(); Err instead of throw.
///
/// Takes `Data` by value and normalizes in place so a 4 MB item is never
/// cloned; only the small uid/name strings are copied.
pub fn validate_data(mut data: Data, label: &str) -> Result<Data, String> {
    if data.items.len() > MAX_STORED_ITEMS {
        return Err(format!(
            "{label}: at most {MAX_STORED_ITEMS} items are allowed"
        ));
    }
    if data.tabs.len() > MAX_TABS {
        return Err(format!("{label}: at most {MAX_TABS} tabs are allowed"));
    }
    let mut total: usize = 0;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (i, entry) in data.items.iter_mut().enumerate() {
        let bytes = get_byte_size(&entry.text);
        if bytes > MAX_ITEM_BYTES {
            return Err(format!("{label}: item {i} is too large (max 4 MB)"));
        }
        total = total.saturating_add(bytes);
        entry.uid = normalize_uid(&entry.uid);
        // Duplicate uids make one delete remove two rows and corrupt tab
        // membership; refuse the write instead of persisting the ambiguity.
        if let Some(uid) = entry.uid.clone() {
            if !seen.insert(uid) {
                return Err(format!("{label}: item {i} has a duplicate id"));
            }
        }
    }
    if total > MAX_TOTAL_BYTES {
        return Err(format!(
            "{label}: the store is too large ({total} bytes, max {MAX_TOTAL_BYTES})"
        ));
    }
    for (i, tab) in data.tabs.iter_mut().enumerate() {
        match normalize_tab(tab) {
            Some(t) => *tab = t,
            None => return Err(format!("{label}: tab {i} is invalid")),
        }
    }
    data.version = 2;
    Ok(data)
}


/// Lenient sanitiser — mirrors sanitizeData(); never fails, drops bad entries.
pub fn sanitize_data(value: &serde_json::Value) -> Data {
    let raw = if value.is_array() {
        serde_json::json!({ "items": value, "tabs": [] })
    } else if value.is_object() {
        value.clone()
    } else {
        return Data::default();
    };

    let mut items: Vec<Entry> = Vec::new();
    let mut total: usize = 0;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(list) = raw.get("items").and_then(|v| v.as_array()) {
        for raw_entry in list.iter().take(MAX_STORED_ITEMS) {
            let entry = if let Some(s) = raw_entry.as_str() {
                Some(Entry { text: s.to_string(), pinned: false, uid: None })
            } else {
                serde_json::from_value::<Entry>(raw_entry.clone()).ok()
            };
            if let Some(mut e) = entry {
                if e.text.is_empty() {
                    continue;
                }
                e.text = truncate_by_bytes(&e.text, MAX_ITEM_BYTES);
                e.uid = normalize_uid(&e.uid);
                let bytes = get_byte_size(&e.text);
                // A file that already grew past the budget (older build, hand
                // edited) must still open: stop adding instead of failing.
                if total.saturating_add(bytes) > MAX_TOTAL_BYTES {
                    continue;
                }
                // Keep the first holder of a uid; the renderer mints a fresh
                // one for the rest on load.
                if let Some(uid) = e.uid.clone() {
                    if !seen.insert(uid) {
                        e.uid = None;
                    }
                }
                total += bytes;
                items.push(e);
            }
        }
    }

    // Stores written by an older build (cap 1000) are trimmed to the new
    // rolling window here, on the very first read.
    let items = apply_retention(items);

    let valid_ids: std::collections::HashSet<String> =
        items.iter().filter_map(|i| i.uid.clone()).collect();

    let mut tabs: Vec<Tab> = Vec::new();
    if let Some(list) = raw.get("tabs").and_then(|v| v.as_array()) {
        for raw_tab in list.iter().take(MAX_TABS) {
            if let Ok(tab) = serde_json::from_value::<Tab>(raw_tab.clone()) {
                if let Some(mut t) = normalize_tab(&tab) {
                    t.ids.retain(|id| valid_ids.contains(id));
                    tabs.push(t);
                }
            }
        }
    }

    Data { version: 2, items, tabs }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The JS side has exactly one copy of these numbers (src/limits.js) and the
    /// Rust side has this one. This test fails the build if they ever drift apart.
    /// Extract `var NAME = <expression>;` from the JS source and evaluate the
    /// simple `a * b * c` product it contains. Formatting (spacing, `var` vs
    /// `const`, comments after the value) no longer breaks the build.
    fn js_const(source: &str, name: &str) -> Option<usize> {
        for line in source.lines() {
            let line = line.trim();
            let Some(rest) = line
                .strip_prefix("var ")
                .or_else(|| line.strip_prefix("const "))
                .or_else(|| line.strip_prefix("let "))
            else {
                continue;
            };
            let mut parts = rest.splitn(2, '=');
            let Some(lhs) = parts.next() else { continue };
            if lhs.trim() != name {
                continue;
            }
            let Some(rhs) = parts.next() else { continue };
            let rhs = rhs.split(';').next().unwrap_or("").split("//").next().unwrap_or("");
            let mut product: usize = 1;
            let mut any = false;
            for factor in rhs.split('*') {
                let f = factor.trim();
                if f.is_empty() {
                    return None;
                }
                let Ok(value) = f.parse::<usize>() else { return None };
                let Some(next) = product.checked_mul(value) else { return None };
                product = next;
                any = true;
            }
            if any {
                return Some(product);
            }
        }
        None
    }

    /// The JS side has exactly one copy of these numbers (src/limits.js) and the
    /// Rust side has this one. This test fails the build if they ever drift apart.
    #[test]
    fn limits_js_parity() {
        const JS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../src/limits.js"));
        let expect = [
            ("MAX_ITEMS", MAX_ITEMS),
            ("MAX_STORED_ITEMS", MAX_STORED_ITEMS),
            ("MAX_ITEM_BYTES", MAX_ITEM_BYTES),
            ("MAX_HEAVY_ITEM_BYTES", MAX_HEAVY_ITEM_BYTES),
            ("MAX_HEAVY_ITEMS", MAX_HEAVY_ITEMS),
            ("MAX_TABS", MAX_TABS),
            ("MAX_TOTAL_BYTES", MAX_TOTAL_BYTES),
            ("MAX_NAME_LENGTH", MAX_NAME_LENGTH),
        ];
        for (name, rust_value) in expect {
            let js_value = js_const(JS, name)
                .unwrap_or_else(|| panic!("src/limits.js does not define {name}"));
            assert_eq!(
                js_value, rust_value,
                "src/limits.js and limits.rs disagree on {name}"
            );
        }
    }
}
