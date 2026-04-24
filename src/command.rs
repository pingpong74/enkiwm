use std::ffi::OsStr;
pub enum Command<S: AsRef<OsStr>> {
    Empty,
    Single(S),
    WithArgs(Vec<S>),
}

impl<S : AsRef<OsStr>> Command<S> {
    pub fn run(self) {
        match self {
            Command::Single(program) => std::process::Command::new(program).spawn().ok(),
            Command::WithArgs(vec) => {
                assert!(vec.len() >= 2);
                let (program, args) = vec.split_first().unwrap();
                std::process::Command::new(program).args(args).spawn().ok()
            }
            _ => None,
        };
    }
}
