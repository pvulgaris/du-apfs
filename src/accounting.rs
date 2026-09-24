use crate::native;
use std::collections::HashMap;
use std::ffi::CString;

#[derive(Default)]
pub(crate) struct Bitmap {
    pages: HashMap<(i32, usize), Box<[u64]>>,
    pub physical: i64,
    pub sparse: bool,
}
impl Bitmap {
    pub fn add(&mut self, dev: i32, r: native::Extent, block: i64) {
        let first = (r.start / block) as usize;
        let last = ((r.end - 1) / block) as usize;
        // Fixed pages avoid allocating a bitmap for gaps in physical addresses
        // or copying its old contents when a high-address extent arrives.
        let words_per_page = if self.sparse { 64 } else { 8192 };
        let first_word = first / 64;
        let last_word = last / 64;
        for page in first_word / words_per_page..=last_word / words_per_page {
            let words = self
                .pages
                .entry((dev, page))
                .or_insert_with(|| vec![0; words_per_page].into_boxed_slice());
            let base = page * words_per_page;
            for i in first_word.max(base)..=last_word.min(base + words_per_page - 1) {
                let low = if i == first_word { first % 64 } else { 0 };
                let high = if i == last_word { last % 64 } else { 63 };
                let mask = (u64::MAX << low) & (u64::MAX >> (63 - high));
                self.physical += (mask & !words[i - base]).count_ones() as i64 * block;
                words[i - base] |= mask;
            }
        }
    }
}

pub(crate) struct Node {
    pub path: CString,
    pub parent: Option<usize>,
    pub operand: usize,
    pub directory: bool,
    pub directory_read_error: bool,
    pub root: bool,
    pub physical: i64,
    pub allocated: i64,
    pub unmeasured: i64,
}

struct Event {
    domain: i32,
    position: i64,
    delta: i8,
    owner: usize,
}

enum Storage {
    Bitmaps(Vec<Bitmap>),
    Events(Vec<Event>),
}

pub(crate) struct Tree {
    pub nodes: Vec<Node>,
    pub domain_bytes: HashMap<i32, i64>,
    storage: Storage,
    single_operand: bool,
}

impl Tree {
    pub fn new(max_depth: Option<usize>, all: bool, operands: usize) -> Self {
        Self {
            nodes: vec![Node {
                path: CString::default(),
                parent: None,
                operand: 0,
                directory: true,
                directory_read_error: false,
                root: false,
                physical: 0,
                allocated: 0,
                unmeasured: 0,
            }],
            domain_bytes: HashMap::new(),
            // Summary and shallow output need only a few unions. Deep output
            // uses a sweep so every directory doesn't need a disk-wide bitmap.
            storage: if max_depth == Some(0) || (!all && max_depth == Some(1)) {
                Storage::Bitmaps(vec![Bitmap::default()])
            } else {
                Storage::Events(Vec::new())
            },
            single_operand: operands == 1,
        }
    }

    pub fn push(&mut self, node: Node) -> usize {
        let id = self.nodes.len();
        if let Storage::Bitmaps(bitmaps) = &mut self.storage {
            bitmaps.push(Bitmap {
                sparse: !node.root,
                ..Bitmap::default()
            });
        }
        self.nodes.push(node);
        id
    }

    pub fn add(&mut self, owner: usize, domain: i32, extent: native::Extent, block: i64) {
        match &mut self.storage {
            Storage::Bitmaps(bitmaps) => {
                let mut next = Some(owner);
                while let Some(id) = next {
                    let parent = self.nodes[id].parent;
                    if id != 0 || !self.single_operand {
                        let before = bitmaps[id].physical;
                        bitmaps[id].add(domain, extent, block);
                        if id == 0 || (self.single_operand && parent == Some(0)) {
                            *self.domain_bytes.entry(domain).or_default() +=
                                bitmaps[id].physical - before;
                        }
                    }
                    next = parent;
                }
            }
            Storage::Events(events) => {
                events.push(Event {
                    domain,
                    position: extent.start,
                    delta: 1,
                    owner,
                });
                events.push(Event {
                    domain,
                    position: extent.end,
                    delta: -1,
                    owner,
                });
            }
        }
    }

    pub fn finish(&mut self) {
        match &mut self.storage {
            Storage::Bitmaps(bitmaps) => {
                for (node, bitmap) in self.nodes.iter_mut().zip(bitmaps.iter()) {
                    node.physical = bitmap.physical;
                }
                if self.single_operand {
                    self.nodes[0].physical =
                        self.nodes.iter().find(|n| n.root).map_or(0, |n| n.physical);
                }
            }
            Storage::Events(events) => {
                events.sort_unstable_by_key(|e| (e.domain, e.position, e.delta));
                let mut active = vec![0usize; self.nodes.len()];
                let mut opened = vec![0i64; self.nodes.len()];
                for event in events.iter() {
                    let mut next = Some(event.owner);
                    while let Some(id) = next {
                        if event.delta > 0 {
                            if active[id] == 0 {
                                opened[id] = event.position;
                            }
                            active[id] += 1;
                        } else {
                            active[id] -= 1;
                            if active[id] == 0 {
                                let bytes = event.position - opened[id];
                                self.nodes[id].physical += bytes;
                                if id == 0 {
                                    *self.domain_bytes.entry(event.domain).or_default() += bytes;
                                }
                            }
                        }
                        next = self.nodes[id].parent;
                    }
                }
                debug_assert!(active.iter().all(|&count| count == 0));
            }
        }
        for id in (1..self.nodes.len()).rev() {
            let parent = self.nodes[id].parent.expect("nonvirtual node has parent");
            self.nodes[parent].allocated += self.nodes[id].allocated;
            self.nodes[parent].unmeasured += self.nodes[id].unmeasured;
        }
        // Output needs only the counts, not the transient extent index.
        self.storage = Storage::Events(Vec::new());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn tree_unions_match_independent_block_sets() {
        for operands in [1, 2] {
            for depth in [None, Some(0), Some(1)] {
                let mut tree = Tree::new(depth, false, operands);
                let first = tree.push(node(0, true));
                let second = tree.push(node(if operands == 1 { first } else { 0 }, operands == 2));
                let child = tree.push(node(first, false));
                let mut expected = vec![HashSet::new(); 4];
                let mut seed = 91u64;
                for _ in 0..250 {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let owner = [first, second, child][(seed % 3) as usize];
                    let domain = 1 + ((seed >> 8) % 2) as i32;
                    let start = (seed >> 16) % 128;
                    let end = start + 1 + (seed >> 24) % 32;
                    tree.add(
                        owner,
                        domain,
                        native::Extent {
                            start: start as i64 * 4096,
                            end: end as i64 * 4096,
                        },
                        4096,
                    );
                    let mut next = Some(owner);
                    while let Some(id) = next {
                        for block in start..end {
                            expected[id].insert((domain, block));
                        }
                        next = tree.nodes[id].parent;
                    }
                }
                tree.nodes[child].allocated = 32768;
                tree.nodes[child].unmeasured = 4096;
                tree.finish();
                for (node, blocks) in tree.nodes.iter().zip(&expected) {
                    assert_eq!(node.physical, blocks.len() as i64 * 4096);
                }
                assert_eq!(tree.nodes[0].allocated, 32768);
                assert_eq!(tree.nodes[0].unmeasured, 4096);
                for domain in [1, 2] {
                    assert_eq!(
                        tree.domain_bytes[&domain],
                        expected[0].iter().filter(|(d, _)| *d == domain).count() as i64 * 4096
                    );
                }
            }
        }
    }

    fn node(parent: usize, root: bool) -> Node {
        Node {
            path: CString::default(),
            parent: Some(parent),
            operand: 0,
            directory: true,
            directory_read_error: false,
            root,
            physical: 0,
            allocated: 0,
            unmeasured: 0,
        }
    }
}
