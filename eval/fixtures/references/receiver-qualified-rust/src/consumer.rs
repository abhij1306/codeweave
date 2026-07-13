use crate::validator::Validator;

impl Validator {
    pub fn apply(&self) {
        self.run_edit_validation();
    }
}
