//! Native official-runtime building blocks. Not wired to production routing.
//! Only a dedicated sandbox worker may call these; a cleared environment and
//! temporary cwd alone are NOT a filesystem/network isolation boundary.
pub mod copilot;
pub mod cursor;
pub mod process;
pub mod state;
