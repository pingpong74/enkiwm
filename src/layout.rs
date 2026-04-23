use smithay::desktop::{Space, Window};
use std::collections::HashSet;

pub struct Grid;

impl Grid {
    pub fn find_first_empty_slot(
        space: &Space<Window>,
        block_width: i32,
        block_height: i32,
    ) -> (i32, i32) {
        let mut occupied = HashSet::new();

        for window in space.elements() {
            if let Some(loc) = space.element_location(window) {
                let idx = (loc.x / block_width, loc.y / block_height);
                occupied.insert(idx);
            }
        }

        let mut target_idx = (0, 0);

        while occupied.contains(&target_idx) {
            target_idx.0 += 1;
        }

        (target_idx.0 * block_width, target_idx.1 * block_height)
    }
}
