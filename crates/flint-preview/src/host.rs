//! The helper's request loop: one framed request in, one framed response
//! out, over stdin/stdout. A malformed request ends the helper (the client
//! restarts it); it never guesses.

use crate::backends::{self, ThumbFn};
use crate::wire::{read_frame, write_frame};
use crate::{Op, Outcome, Request, Response};
use std::io::{Read, Write};

/// Environment variable that enables test-only failure hooks: a request
/// for a path ending in `__flint_crash__` aborts the helper, one ending in
/// `__flint_hang__` never answers. Off unless the variable is "1".
pub const TEST_HOOKS_ENV: &str = "FLINT_PREVIEW_TEST_HOOKS";

pub fn handle(req: &Request, thumb: Option<ThumbFn>) -> Outcome {
    let r = match req.op {
        Op::Preview => backends::preview(&req.path, &req.limits, thumb).map(Outcome::Preview),
        Op::Extract => backends::extract(&req.path, &req.limits).map(Outcome::Extracted),
    };
    r.unwrap_or_else(Outcome::Failed)
}

pub fn serve(mut input: impl Read, mut output: impl Write, thumb: Option<ThumbFn>) -> Result<(), String> {
    let hooks = std::env::var(TEST_HOOKS_ENV).as_deref() == Ok("1");
    loop {
        let Some(frame) = read_frame(&mut input).map_err(|e| e.to_string())? else { return Ok(()) };
        let req = Request::decode(&frame).map_err(|e| e.to_string())?;
        if hooks && req.path.ends_with("__flint_crash__") {
            std::process::abort();
        }
        if hooks && req.path.ends_with("__flint_hang__") {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
        // A backend panic is a bug, but it must cost one file, not the
        // helper's other requests.
        let outcome = std::panic::catch_unwind(|| handle(&req, thumb)).unwrap_or_else(|_| Outcome::Failed("internal error while reading this file".into()));
        write_frame(&mut output, &Response { id: req.id, outcome }.encode()).map_err(|e| e.to_string())?;
    }
}
