//! Lists the screen keeps, updated in place.
//!
//! Handing Slint a new model on every render recreates every row, and a row's
//! `changed` handlers (keeping the focused row in view) only fire on a row that
//! already exists. So each list keeps one model, and a render changes only the
//! rows whose data changed, adding or removing rows at the end.

use std::rc::Rc;

use slint::{Model, ModelRc, VecModel};

/// One list's model, kept for the window's life.
pub struct Kept<T: Clone + 'static> {
    model: Rc<VecModel<T>>,
}

impl<T: Clone + PartialEq + 'static> Kept<T> {
    pub fn new() -> Self {
        Kept { model: Rc::new(VecModel::default()) }
    }

    pub fn model(&self) -> ModelRc<T> {
        ModelRc::from(self.model.clone())
    }

    /// Make the model `rows`, touching only what differs.
    pub fn set(&self, rows: Vec<T>) {
        let old = self.model.row_count();
        for (i, row) in rows.iter().enumerate() {
            if i < old {
                if self.model.row_data(i).as_ref() != Some(row) {
                    self.model.set_row_data(i, row.clone());
                }
            } else {
                self.model.push(row.clone());
            }
        }
        for i in (rows.len()..old).rev() {
            self.model.remove(i);
        }
    }
}
