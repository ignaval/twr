//! Hash sets serialized in a fixed order (`#[serde(serialize_with = ...)]`), so that equal
//! states serialize to equal bytes: the hub's checkpoints of its folded state are compared that
//! way in tests (`twr_hub`'s `hub::checkpoint`). Maps need none of this when they go through
//! [`serde_json::Value`], whose objects are sorted by key.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Serialize, Serializer};

/// `set` as a sorted sequence (it deserializes as a set does).
pub fn set<T: Ord + Serialize, S: Serializer>(set: &HashSet<T>, s: S) -> Result<S::Ok, S::Error> {
    set.iter().collect::<BTreeSet<_>>().serialize(s)
}

/// `map` sorted by key, each of its sets as a sorted sequence.
pub fn map_of_sets<K: Ord + Serialize, T: Ord + Serialize, S: Serializer>(
    map: &HashMap<K, HashSet<T>>,
    s: S,
) -> Result<S::Ok, S::Error> {
    map.iter()
        .map(|(k, v)| (k, v.iter().collect::<BTreeSet<_>>()))
        .collect::<BTreeMap<_, _>>()
        .serialize(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, serde::Deserialize, PartialEq, Debug)]
    struct S {
        #[serde(serialize_with = "set")]
        a: HashSet<u64>,
        #[serde(serialize_with = "map_of_sets")]
        b: HashMap<String, HashSet<String>>,
    }

    #[test]
    fn sets_serialize_sorted_and_read_back() {
        let s = S {
            a: (0..50).rev().collect(),
            b: [("y", ["q", "p"]), ("x", ["b", "a"])]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
                .collect(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let a: Vec<String> = (0..50).map(|n| n.to_string()).collect();
        assert!(
            json.starts_with(&format!("{{\"a\":[{}]", a.join(","))),
            "{json}"
        );
        assert!(
            json.ends_with(r#""b":{"x":["a","b"],"y":["p","q"]}}"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<S>(&json).unwrap(), s);
    }
}
