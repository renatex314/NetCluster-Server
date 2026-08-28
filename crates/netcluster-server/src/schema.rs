//! Filter schema: dimensions, query shapes, and the cell encoding.
//!
//! A *dimension* is a property you filter on (`client`, `status`). A *shape* is a
//! combination you are allowed to query (`["client", "status"]`). A *cell* is one
//! concrete assignment of values to the dimensions of one shape. The index itself
//! deals only in cell integers; naming lives here.
//!
//! Shapes are declared rather than inferred because they are what costs memory: a
//! device contributes one aggregate entry per shape per tree level, so declaring
//! `[["client"], ["status"], ["client","status"]]` costs three times what
//! `[["client"]]` does. Inferring them from whatever a client happened to ask for
//! would make a collection's footprint depend on which page someone opened.
//!
//! A query must name exactly the dimensions of some declared shape. That is what
//! keeps a filtered query at one lookup per node: matching a subset of a cross
//! product would mean summing every cell that agrees on the named dimensions, and
//! there are more of those the fewer dimensions you name.

use netcluster::MAX_CELLS;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One filterable property.
///
/// Give it either `values` (the labels, when you know them) or `capacity` (how
/// many distinct ones may exist, when you do not). With `capacity` the values are
/// *interned*: each one seen for the first time takes the next free index, so the
/// ceiling is how many can coexist rather than how large an id may get. A fleet
/// with auto-increment client ids running into the millions but two thousand live
/// clients wants `capacity: 4096`, not a list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Dimension {
    pub name: String,
    /// Value labels; a label's position in this list is its value index.
    #[serde(default)]
    pub values: Vec<String>,
    /// How many distinct values may exist, when they are not known up front.
    /// Mutually exclusive with `values`.
    #[serde(default)]
    pub capacity: Option<usize>,
    /// May one device hold several of these values at once? A vehicle owned by
    /// three clients cannot be expressed any other way.
    #[serde(default)]
    pub multi: bool,
}

impl Dimension {
    /// How many values this dimension can take: its declared list, or its cap.
    pub fn size(&self) -> usize {
        self.capacity.unwrap_or(self.values.len())
    }
    pub fn is_dynamic(&self) -> bool {
        self.capacity.is_some()
    }
}

/// Resolves a value to its index. Reporting may create one; querying may not.
pub trait Values {
    /// `Ok(None)` means "legal, but nothing has ever had this value" -- a real
    /// answer for a dynamic dimension, and one a query must treat as an empty
    /// result rather than an error.
    fn get(&mut self, d: usize, v: &str) -> Result<Option<u32>, String>;
}

#[derive(Clone, Debug, Default)]
pub struct Shape {
    pub dims: Vec<usize>,
    pub base: u32,
    pub strides: Vec<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct Schema {
    pub dims: Vec<Dimension>,
    pub shapes: Vec<Shape>,
    pub cells: usize,
    pub max_cells_per_device: usize,
    by_name: HashMap<String, usize>,
    by_key: HashMap<String, usize>,
    /// label -> index for declared dimensions; empty for dynamic ones.
    labels: Vec<HashMap<String, u32>>,
}

impl Schema {
    /// Build from declared dimensions and shapes.
    ///
    /// `filters` empty means "each dimension on its own", which reproduces a
    /// single category both in behaviour and in cost.
    pub fn new(dims: Vec<Dimension>, filters: &[Vec<String>]) -> Result<Self, String> {
        let mut by_name = HashMap::new();
        for (i, d) in dims.iter().enumerate() {
            match (d.values.is_empty(), d.capacity) {
                (true, None) => {
                    return Err(format!(
                        "dimension {:?} declares neither `values` nor `capacity`",
                        d.name
                    ))
                }
                (false, Some(_)) => {
                    return Err(format!(
                        "dimension {:?} declares both `values` and `capacity`; \
                         list the labels or give a ceiling, not both",
                        d.name
                    ))
                }
                (true, Some(0)) => return Err(format!("dimension {:?} has capacity 0", d.name)),
                _ => {}
            }
            let mut seen = std::collections::HashSet::new();
            for v in &d.values {
                if !seen.insert(v) {
                    return Err(format!("dimension {:?} repeats the value {:?}", d.name, v));
                }
            }
            if by_name.insert(d.name.clone(), i).is_some() {
                return Err(format!("duplicate dimension {:?}", d.name));
            }
        }

        let raw: Vec<Vec<String>> = if filters.is_empty() {
            dims.iter().map(|d| vec![d.name.clone()]).collect()
        } else {
            filters.to_vec()
        };

        let mut shapes: Vec<Shape> = Vec::new();
        let mut by_key: HashMap<String, usize> = HashMap::new();
        for f in &raw {
            if f.is_empty() {
                return Err("a filter shape must name at least one dimension".into());
            }
            let mut idx = Vec::with_capacity(f.len());
            for n in f {
                match by_name.get(n) {
                    Some(&i) => idx.push(i),
                    None => {
                        return Err(format!(
                            "filter shape names {n:?}, which is not a declared dimension \
                             (have: {})",
                            dims.iter()
                                .map(|d| d.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                    }
                }
            }
            // Sorted, so ["a","b"] and ["b","a"] are one shape rather than two
            // that silently double the memory.
            idx.sort_unstable();
            if idx.windows(2).any(|w| w[0] == w[1]) {
                return Err(format!("filter shape {f:?} repeats a dimension"));
            }
            let key = idx
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",");
            if by_key.contains_key(&key) {
                return Err(format!("duplicate filter shape {f:?}"));
            }
            by_key.insert(key, shapes.len());
            shapes.push(Shape {
                dims: idx,
                base: 0,
                strides: Vec::new(),
            });
        }

        // Each shape gets a contiguous block, so a cell is `base + mixed-radix
        // index` and never needs decoding.
        let mut base: u64 = 0;
        for sh in &mut shapes {
            sh.base = base as u32;
            let mut stride: u64 = 1;
            sh.strides = vec![0; sh.dims.len()];
            for k in (0..sh.dims.len()).rev() {
                sh.strides[k] = stride as u32;
                stride *= dims[sh.dims[k]].size() as u64;
                if stride > MAX_CELLS as u64 {
                    return Err(format!(
                        "filter shape {:?} needs more than the {MAX_CELLS} cells available; \
                         a shape costs the product of its dimensions",
                        sh.dims
                            .iter()
                            .map(|&i| dims[i].name.as_str())
                            .collect::<Vec<_>>()
                    ));
                }
            }
            base += stride;
        }
        if base > MAX_CELLS as u64 {
            return Err(format!(
                "the declared filters need {base} cells, over the {MAX_CELLS} limit"
            ));
        }

        // A device holds one cell per shape when every value is single; a
        // multi-valued dimension multiplies. Bound it, so one bad device cannot
        // quietly cost a hundred times its neighbours.
        let worst: usize = shapes
            .iter()
            .map(|sh| {
                sh.dims
                    .iter()
                    .map(|&i| if dims[i].multi { 4 } else { 1 })
                    .product::<usize>()
            })
            .sum();
        let labels = dims
            .iter()
            .map(|d| {
                d.values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (v.clone(), i as u32))
                    .collect()
            })
            .collect();
        Ok(Schema {
            cells: base as usize,
            max_cells_per_device: worst.max(1),
            dims,
            shapes,
            by_name,
            by_key,
            labels,
        })
    }

    pub fn enabled(&self) -> bool {
        !self.dims.is_empty()
    }

    /// Resolve a value on a *declared* dimension. Dynamic ones are the
    /// interner's business and never reach here.
    ///
    /// A hash lookup, not a scan: this runs once per dimension per device on
    /// every report, so a collection with a few thousand labels would otherwise
    /// pay for all of them on each one.
    pub fn static_value(&self, d: usize, v: &str) -> Result<u32, String> {
        let dim = &self.dims[d];
        if let Some(&i) = self.labels[d].get(v) {
            return Ok(i);
        }
        if let Ok(n) = v.parse::<usize>() {
            if n < dim.values.len() {
                return Ok(n as u32);
            }
        }
        Err(format!(
            "unknown value {v:?} for {:?}; this collection has {:?}",
            dim.name, dim.values
        ))
    }

    /// Every cell a device holding `vals` belongs to.
    ///
    /// A dimension absent from `vals` takes value 0, which is what a missing
    /// `category` has always meant. Declare an explicit "unassigned" label if that
    /// matters.
    pub fn cells_for<V: Values>(
        &self,
        vals: &HashMap<String, Vec<String>>,
        out: &mut Vec<u32>,
        res: &mut V,
    ) -> Result<(), String> {
        out.clear();
        if !self.enabled() {
            return Ok(());
        }
        let mut resolved: Vec<Vec<u32>> = Vec::with_capacity(self.dims.len());
        for (d, dim) in self.dims.iter().enumerate() {
            match vals.get(&dim.name) {
                None => resolved.push(vec![0]),
                Some(list) if list.is_empty() => resolved.push(vec![0]),
                Some(list) => {
                    if !dim.multi && list.len() > 1 {
                        return Err(format!(
                            "{:?} got {} values but is not declared multi",
                            dim.name,
                            list.len()
                        ));
                    }
                    let mut v = Vec::with_capacity(list.len());
                    for x in list {
                        // A report is what creates a value, so an unseen one on a
                        // dynamic dimension is interned rather than refused.
                        let Some(i) = res.get(d, x)? else {
                            return Err(format!(
                                "value {x:?} for {:?} could not be resolved",
                                dim.name
                            ));
                        };
                        if !v.contains(&i) {
                            v.push(i);
                        }
                    }
                    resolved.push(v);
                }
            }
        }
        for sh in &self.shapes {
            emit(sh, &resolved, 0, sh.base, out);
        }
        if out.len() > self.max_cells_per_device {
            return Err(format!(
                "lands in {} filter cells, over the {} this collection allows",
                out.len(),
                self.max_cells_per_device
            ));
        }
        Ok(())
    }

    /// The single cell a query selects, or -1 for "everything".
    ///
    /// `sel` must name exactly the dimensions of a declared shape; anything else
    /// is an error rather than a slow path, so a filter can never quietly become a
    /// scan of the viewport.
    pub fn query_cell<V: Values>(
        &self,
        sel: &HashMap<String, String>,
        res: &mut V,
    ) -> Result<Option<i32>, String> {
        if sel.is_empty() {
            return Ok(Some(-1));
        }
        let mut idx = Vec::with_capacity(sel.len());
        for n in sel.keys() {
            match self.by_name.get(n) {
                Some(&i) => idx.push(i),
                None => {
                    return Err(format!(
                        "unknown filter {n:?}; this collection has {}",
                        self.dims
                            .iter()
                            .map(|d| d.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }
            }
        }
        idx.sort_unstable();
        let key = idx
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let Some(&si) = self.by_key.get(&key) else {
            // Named in declaration order rather than the map's, so the message is
            // the same every time it is produced -- it ends up in logs and tests.
            let named: Vec<&str> = idx.iter().map(|&i| self.dims[i].name.as_str()).collect();
            return Err(format!(
                "no declared filter combines [{}]; this collection allows {}",
                named.join(", "),
                self.describe_shapes()
            ));
        };
        let sh = &self.shapes[si];
        let mut cell = sh.base;
        for (k, &d) in sh.dims.iter().enumerate() {
            let v = &sel[&self.dims[d].name];
            // Nothing has ever had this value. That is an answer -- an empty map --
            // not an error: on a dynamic dimension the caller cannot know which
            // values exist, so refusing would make every new client a 400 until
            // its first vehicle reported.
            let Some(i) = res.get(d, v)? else {
                return Ok(None);
            };
            cell += i * sh.strides[k];
        }
        Ok(Some(cell as i32))
    }

    pub fn describe_shapes(&self) -> String {
        self.shapes
            .iter()
            .map(|sh| {
                format!(
                    "[{}]",
                    sh.dims
                        .iter()
                        .map(|&i| self.dims[i].name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Value indices for dimensions whose values are not known up front.
///
/// One table per dimension; declared dimensions have an empty one and never touch
/// it. Assignment is arrival order, which means two replicas fed the same stream
/// may give the same client different indices -- harmless, because a name is
/// resolved against the same table that answers the query, and cluster ids
/// already differ between replicas.
#[derive(Clone, Debug, Default)]
pub struct Interner {
    tables: Vec<HashMap<String, u32>>,
}

impl Interner {
    pub fn new(schema: &Schema) -> Self {
        Interner {
            tables: vec![HashMap::new(); schema.dims.len()],
        }
    }

    /// index -> value per dimension, for the snapshot. Without this a restore
    /// would keep the cells and lose what they meant.
    pub fn labels(&self) -> Vec<Vec<String>> {
        self.tables
            .iter()
            .map(|t| {
                let mut v = vec![String::new(); t.len()];
                for (s, &i) in t {
                    v[i as usize] = s.clone();
                }
                v
            })
            .collect()
    }

    /// Rebuild from a snapshot, keeping every index exactly where it was.
    pub fn restore(schema: &Schema, labels: &[Vec<String>]) -> Self {
        let mut me = Interner::new(schema);
        for (d, list) in labels.iter().enumerate() {
            if d >= me.tables.len() || !schema.dims[d].is_dynamic() {
                continue;
            }
            for (i, v) in list.iter().enumerate() {
                if !v.is_empty() && i < schema.dims[d].size() {
                    me.tables[d].insert(v.clone(), i as u32);
                }
            }
        }
        me
    }

    pub fn len(&self, d: usize) -> usize {
        self.tables[d].len()
    }
}

/// Reporting: an unseen value takes the next free index.
pub struct Interning<'a> {
    pub schema: &'a Schema,
    pub interner: &'a mut Interner,
}

impl Values for Interning<'_> {
    fn get(&mut self, d: usize, v: &str) -> Result<Option<u32>, String> {
        let dim = &self.schema.dims[d];
        if !dim.is_dynamic() {
            return self.schema.static_value(d, v).map(Some);
        }
        let t = &mut self.interner.tables[d];
        if let Some(&i) = t.get(v) {
            return Ok(Some(i));
        }
        let next = t.len();
        if next >= dim.size() {
            return Err(format!(
                "{:?} already holds {} distinct values, its declared capacity; \
                 {v:?} would be one more",
                dim.name,
                dim.size()
            ));
        }
        t.insert(v.to_string(), next as u32);
        Ok(Some(next as u32))
    }
}

/// Querying: an unseen value resolves to nothing, and creates nothing.
pub struct Looking<'a> {
    pub schema: &'a Schema,
    pub interner: &'a Interner,
}

impl Values for Looking<'_> {
    fn get(&mut self, d: usize, v: &str) -> Result<Option<u32>, String> {
        let dim = &self.schema.dims[d];
        if !dim.is_dynamic() {
            return self.schema.static_value(d, v).map(Some);
        }
        Ok(self.interner.tables[d].get(v).copied())
    }
}

fn emit(sh: &Shape, resolved: &[Vec<u32>], k: usize, acc: u32, out: &mut Vec<u32>) {
    if k == sh.dims.len() {
        out.push(acc);
        return;
    }
    for &v in &resolved[sh.dims[k]] {
        emit(sh, resolved, k + 1, acc + v * sh.strides[k], out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolve a query against a fresh interner; declared dimensions need none.
    fn query(s: &Schema, sel: &HashMap<String, String>) -> Result<i32, String> {
        let interner = Interner::new(s);
        s.query_cell(
            sel,
            &mut Looking {
                schema: s,
                interner: &interner,
            },
        )
        .map(|c| c.expect("declared values always resolve"))
    }

    fn dim(name: &str, values: &[&str], multi: bool) -> Dimension {
        Dimension {
            name: name.into(),
            values: values.iter().map(|s| s.to_string()).collect(),
            capacity: None,
            multi,
        }
    }

    fn sample() -> Schema {
        Schema::new(
            vec![
                dim("client", &["a", "b", "c"], true),
                dim("status", &["idle", "enroute"], false),
            ],
            &[
                vec!["client".into()],
                vec!["status".into()],
                vec!["client".into(), "status".into()],
            ],
        )
        .unwrap()
    }

    #[test]
    fn cells_are_blocked_per_shape() {
        let s = sample();
        assert_eq!(s.cells, 3 + 2 + 6);
    }

    #[test]
    fn a_multi_valued_device_lands_in_a_cell_per_value() {
        let s = sample();
        let mut vals = HashMap::new();
        vals.insert("client".into(), vec!["a".into(), "c".into()]);
        vals.insert("status".into(), vec!["enroute".into()]);
        let mut out = Vec::new();
        let mut interner = Interner::new(&s);
        s.cells_for(
            &vals,
            &mut out,
            &mut Interning {
                schema: &s,
                interner: &mut interner,
            },
        )
        .unwrap();
        out.sort_unstable();
        // client a, client c, status enroute, (a,enroute), (c,enroute)
        assert_eq!(out.len(), 5);

        let mut sel = HashMap::new();
        sel.insert("client".to_string(), "a".to_string());
        sel.insert("status".to_string(), "enroute".to_string());
        assert!(out.contains(&(query(&s, &sel).unwrap() as u32)));

        sel.insert("client".to_string(), "b".to_string());
        assert!(!out.contains(&(query(&s, &sel).unwrap() as u32)));
    }

    #[test]
    fn an_undeclared_combination_is_refused_rather_than_scanned() {
        let s = Schema::new(
            vec![
                dim("client", &["a", "b"], false),
                dim("status", &["x"], false),
            ],
            &[vec!["client".into()], vec!["status".into()]],
        )
        .unwrap();
        let mut sel = HashMap::new();
        sel.insert("client".to_string(), "a".to_string());
        sel.insert("status".to_string(), "x".to_string());
        let err = query(&s, &sel).unwrap_err();
        assert!(err.contains("no declared filter combines"), "{err}");
    }

    #[test]
    fn unknown_names_and_values_are_named_in_the_error() {
        let s = sample();
        let mut sel = HashMap::new();
        sel.insert("nope".to_string(), "a".to_string());
        assert!(query(&s, &sel).unwrap_err().contains("unknown filter"));

        let mut sel = HashMap::new();
        sel.insert("status".to_string(), "gone".to_string());
        assert!(query(&s, &sel).unwrap_err().contains("unknown value"));
    }

    #[test]
    fn an_empty_selection_means_everything() {
        assert_eq!(query(&sample(), &HashMap::new()).unwrap(), -1);
    }

    #[test]
    fn a_single_valued_dimension_refuses_a_list() {
        let s = sample();
        let mut vals = HashMap::new();
        vals.insert("status".into(), vec!["idle".into(), "enroute".into()]);
        let mut interner = Interner::new(&s);
        let err = s
            .cells_for(
                &vals,
                &mut Vec::new(),
                &mut Interning {
                    schema: &s,
                    interner: &mut interner,
                },
            )
            .unwrap_err();
        assert!(err.contains("not declared multi"), "{err}");
    }

    #[test]
    fn a_missing_dimension_takes_value_zero() {
        let s = sample();
        let mut out = Vec::new();
        let mut interner = Interner::new(&s);
        s.cells_for(
            &HashMap::new(),
            &mut out,
            &mut Interning {
                schema: &s,
                interner: &mut interner,
            },
        )
        .unwrap();
        let mut sel = HashMap::new();
        sel.insert("client".to_string(), "a".to_string());
        sel.insert("status".to_string(), "idle".to_string());
        assert!(out.contains(&(query(&s, &sel).unwrap() as u32)));
    }

    #[test]
    fn duplicate_declarations_are_refused() {
        assert!(Schema::new(vec![dim("a", &["x"], false), dim("a", &["y"], false)], &[]).is_err());
        assert!(Schema::new(vec![dim("a", &["x", "x"], false)], &[]).is_err());
        assert!(Schema::new(
            vec![dim("a", &["x"], false)],
            &[vec!["a".into()], vec!["a".into()]]
        )
        .is_err());
        assert!(Schema::new(vec![dim("a", &["x"], false)], &[vec!["b".into()]]).is_err());
    }
}
