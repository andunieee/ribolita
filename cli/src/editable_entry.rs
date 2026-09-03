use ribolita_common::{entry::Entry, rmp_json};

use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EditableEntry {
    pub id: u64,
    pub when: u64,
    pub kind: String,
    pub values: HashMap<String, serde_json::Value>,

    #[serde(skip)]
    pub is_atomic: HashSet<String>,
}

impl From<Entry> for EditableEntry {
    fn from(entry: Entry) -> Self {
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
            id: entry.id,
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

impl From<EditableEntry> for Entry {
    fn from(val: EditableEntry) -> Self {
        let mut values = Vec::with_capacity(val.values.len() * 2 - val.is_atomic.len());

        for (k, v) in val.values {
            let rmpval = rmp_json::json_value_to_rmp_value(&v);

            if val.is_atomic.contains(&k) {
                values.push((k, rmpval));
            } else if let rmpv::Value::Array(rmpvals) = rmpval {
                for rmpval in rmpvals {
                    values.push((k.clone(), rmpval));
                }
            } else {
                panic!("{:?} should have been an array", rmpval)
            }
        }

        Entry {
            id: val.id,
            when: val.when,
            kind: val.kind,
            values,
        }
    }
}

impl From<&EditableEntry> for Entry {
    fn from(val: &EditableEntry) -> Self {
        let mut values = Vec::with_capacity(val.values.len() * 2 - val.is_atomic.len());

        for (k, v) in val.values.iter() {
            let rmpval = rmp_json::json_value_to_rmp_value(v);

            if val.is_atomic.contains(k) {
                values.push((k.clone(), rmpval));
            } else if let rmpv::Value::Array(rmpvals) = rmpval {
                for rmpval in rmpvals {
                    values.push((k.clone(), rmpval));
                }
            } else {
                panic!("{:?} should have been an array", rmpval)
            }
        }

        Entry {
            id: val.id,
            when: val.when,
            kind: val.kind.clone(),
            values,
        }
    }
}

impl From<&mut EditableEntry> for Entry {
    fn from(val: &mut EditableEntry) -> Self {
        (val as &EditableEntry).into()
    }
}
