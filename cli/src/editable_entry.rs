use ribolita_common::{entry::Entry, rmp_json};

use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EditableEntry {
    /// the entry's id when it was loaded, used to match edits to entries
    pub id: String,
    /// when the event happened, in seconds since the unix epoch
    pub when: u64,
    pub kind: String,
    pub values: HashMap<String, serde_json::Value>,

    #[serde(skip)]
    pub is_atomic: HashSet<String>,
}

impl From<Entry> for EditableEntry {
    fn from(entry: Entry) -> Self {
        let id = entry.id.to_string();
        let mut values: HashMap<String, serde_json::Value> =
            HashMap::with_capacity(entry.values.len());

        // means this hasn't been converted into an array yet
        // (we will convert values into arrays when they show up twice -- but we have to be able to
        //  distinguish between natural "atomic" arrays, if they were already arrays originally
        //  and arrays we made just for the editing)
        let mut is_atomic = HashSet::with_capacity(entry.values.len());

        for (k, v) in entry.values {
            let jval = rmp_json::rmp_value_to_json_value(&v);

            if let Some(curr) = values.get(&k) {
                if is_atomic.contains(&k) {
                    // turn it into an array
                    is_atomic.remove(&k);
                    values.insert(k, serde_json::Value::Array(vec![curr.clone(), jval]));
                } else {
                    // it's already an array, so just push to it
                    if let Some(serde_json::Value::Array(curr)) = values.get_mut(&k) {
                        curr.push(jval);
                    } else {
                        panic!("{:?} should be an array by now", values.get(&k));
                    }
                }
            } else {
                values.insert(k.clone(), jval);
                is_atomic.insert(k);
            }
        }

        EditableEntry {
            id,
            when: entry.when,
            kind: entry.kind,
            values,
            is_atomic,
        }
    }
}

// do not check is_atomic
impl PartialEq<EditableEntry> for EditableEntry {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.when == other.when
            && self.kind == other.kind
            && self.values == other.values
    }
}

impl EditableEntry {
    /// The edited `when`, kind and values, with values that were split into arrays for editing
    /// turned back into repeated keys.
    pub fn to_parts(&self) -> (u64, String, Vec<(String, rmpv::Value)>) {
        let mut values = Vec::with_capacity(self.values.len());
        for (k, v) in self.values.iter() {
            let rmpval = rmp_json::json_value_to_rmp_value(v);
            match rmpval {
                rmpv::Value::Array(rmpvals) if !self.is_atomic.contains(k) => {
                    for rmpval in rmpvals {
                        values.push((k.clone(), rmpval));
                    }
                }
                rmpval => values.push((k.clone(), rmpval)),
            }
        }
        (self.when, self.kind.clone(), values)
    }
}
