use std::fmt;

/// A failure of lest itself, as opposed to failing tests. Tool errors exit
/// with code 2; test failures exit with code 1. The two are never conflated.
#[derive(Debug)]
pub struct ToolError(pub String);

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ToolError {}
