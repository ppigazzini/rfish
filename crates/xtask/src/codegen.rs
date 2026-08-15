//! Prove a refactor emits the same machine code, symbol by symbol.
//!
//! **The gate for the commit message that says "no functional change".** Every other
//! instrument here answers a question about BEHAVIOUR — `signature` pins the node count,
//! the goldens pin the output, `nnue-check` pins the network's answer — and all of them
//! stay green when a rewrite keeps the behaviour and costs instructions. `perf-budget`
//! would see that, but only above its tolerance, only on the tier callgrind can execute,
//! and only after a per-machine golden has been derived. None of them can say the thing a
//! refactor actually claims, which is that the compiler emitted what it emitted before.
//!
//! This compares the DISASSEMBLY of the working tree against a git ref, per symbol, at one
//! tier. A refactor that is genuinely a rewording reports every symbol identical in seconds;
//! one that moved a bound, changed an inlining decision or gave the register allocator a
//! different problem names the symbols it moved.
//!
//! Ported from `../Stockfish`'s `refish` branch — `d4324d9e` built it and `f1318daa`
//! corrected it, the correction being that **alignment padding is not codegen**: a function
//! whose body is unchanged still ends on a different number of `nop`s when the function
//! before it changed length, and counting those reported a diff on every symbol downstream
//! of a real one.
//!
//! # What it cannot say
//!
//! Identical code is not identical behaviour when the change is in data. A different
//! constant in a table, a different weight file, a different `static` — none of it appears
//! here, because none of it is in `.text`. This proves the compiler's output for the code,
//! and `signature` remains the gate that proves the engine.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::runner::{Outcome, cargo};
use crate::{have, run, workspace_root};

/// Where the baseline checkout and both builds live.
///
/// Under `target/`, so it is gitignored and a `cargo clean` takes it with everything else.
const SCRATCH: &str = "target/codegen";

/// One symbol's normalised instruction stream.
struct Symbol {
    name: String,
    body: Vec<String>,
}

/// Prove the working tree's codegen against `--base` (default `HEAD`).
pub(crate) fn codegen_equiv(args: &[&str]) -> Result<Outcome, String> {
    if !have("objdump") {
        return Ok(Outcome::Skipped("objdump is needed to read the disassembly".to_string()));
    }
    let base = arg_value(args, "--base").unwrap_or("HEAD");
    let tier = crate::perf::tier_for(args)?;

    // **REFUSE A COMPARISON OF A TREE WITH ITSELF.** With a clean checkout the two sides are
    // the same source, so every symbol matches and the gate reports a proof it never made —
    // the failure shape `compared_something` exists for one file over, arriving here through
    // the default argument rather than through a missing corpus.
    let changed = crate::runner::tracked_rust_diff(base)?;
    if changed.is_empty() {
        return Ok(Outcome::Skipped(format!(
            "nothing to prove: no tracked Rust source differs from {base}. This step compares \
             the WORKING TREE against a ref, so on a clean checkout it would compare a tree \
             with itself"
        )));
    }
    println!(
        "codegen-equiv: {} changed file(s) against {base}, at tier {} ({})",
        changed.len(),
        tier.name,
        tier.rustc
    );
    for f in &changed {
        println!("  {f}");
    }

    let scratch = workspace_root().join(SCRATCH);
    std::fs::create_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;

    // The baseline is built from a DETACHED worktree at the ref, never by checking anything
    // out here: a gate that moves the working tree is a gate that can lose the work it was
    // asked to judge. AGENTS.md's rule about a worktree starting where its branch last was
    // is why this detaches at an explicit ref rather than naming a branch.
    let tree = scratch.join("base-tree");
    crate::runner::worktree_at(base, &tree)?;

    let flags = format!("-C target-cpu={}", tier.rustc);
    let base_bin = build_at(&tree, &scratch.join("base-target"), &flags)?;
    let head_bin = build_at(&workspace_root(), &scratch.join("head-target"), &flags)?;

    let before = disassemble(&base_bin)?;
    let after = disassemble(&head_bin)?;

    // Leave no worktree behind: the next run adds its own, and a stale one at a stale ref is
    // a baseline nobody chose.
    crate::runner::worktree_remove(&tree);

    Ok(report(&before, &after))
}

/// Build one side and return its binary.
///
/// `profiling` rather than `release`: it inherits every release codegen setting — fat LTO,
/// one codegen unit, the same opt level — and only stops the linker throwing the symbol
/// table away. A stripped binary has no symbols to compare, and comparing whole `.text`
/// blobs would report one diff for any change anywhere.
fn build_at(src: &Path, target_dir: &Path, flags: &str) -> Result<PathBuf, String> {
    println!("codegen-equiv: building {}", src.display());
    run(Command::new(cargo())
        .current_dir(src)
        .env("RUSTFLAGS", flags)
        .env("CARGO_TARGET_DIR", target_dir)
        .args(["build", "--profile", "profiling", "--package", "rfish", "--bin", "stockfish"]))?;
    let bin = target_dir.join("profiling").join(crate::runner::engine_file_name());
    if !bin.is_file() {
        return Err(format!("{} was not produced", bin.display()));
    }
    Ok(bin)
}

/// Disassemble `.text` into normalised symbols.
fn disassemble(bin: &Path) -> Result<Vec<Symbol>, String> {
    let out = Command::new("objdump")
        .args(["-d", "--no-show-raw-insn", "--section=.text"])
        .arg(bin)
        .output()
        .map_err(|e| format!("objdump: {e}"))?;
    if !out.status.success() {
        return Err(format!("objdump on {} failed", bin.display()));
    }
    let text = String::from_utf8_lossy(&out.stdout);

    let mut symbols: Vec<Symbol> = Vec::new();
    for line in text.lines() {
        if let Some(name) = symbol_header(line) {
            symbols.push(Symbol { name, body: Vec::new() });
        } else if let Some(insn) = instruction(line)
            && let Some(sym) = symbols.last_mut()
        {
            sym.body.push(insn);
        }
    }
    for sym in &mut symbols {
        trim_padding(&mut sym.body);
    }
    if symbols.is_empty() {
        return Err(format!("{} disassembled to no symbols at all", bin.display()));
    }
    Ok(symbols)
}

/// `0000000000401234 <name>:` to a normalised `name`.
fn symbol_header(line: &str) -> Option<String> {
    let rest = line.strip_suffix(">:")?;
    let open = rest.find(" <")?;
    Some(normalise_symbol(&rest[open + 2..]))
}

/// One disassembly line to its normalised instruction text, or `None` for anything else.
fn instruction(line: &str) -> Option<String> {
    let (addr, rest) = line.split_once('\t')?;
    // The address column is `  401234:` and nothing else; a continuation line has no colon.
    if !addr.trim_end().ends_with(':') {
        return None;
    }
    Some(normalise_operands(rest))
}

/// Strip the parts of an instruction that record LAYOUT rather than work.
///
/// Three substitutions, and each is a place where identical code prints differently once
/// something before it changes size:
///
/// - a branch or call target prints as an absolute address followed by the symbol; the
///   symbol is the content and the address is where it landed;
/// - a rip-relative operand prints a displacement plus a resolved comment; again the
///   comment names the datum and the displacement is where the datum landed;
/// - a bare absolute address with no symbol becomes `ADDR`, because nothing else about it
///   is comparable across two links.
fn normalise_operands(insn: &str) -> String {
    let mut s = insn.trim().to_string();

    // `call 401000 <sym>` and `mov 0x1234(%rip),%rax  # 402000 <sym>` both carry the target
    // in a trailing `<sym>`. Take it, drop the numbers, and keep the mnemonic and registers.
    if let Some(open) = s.rfind(" <") {
        if let Some(close) = s[open..].find('>') {
            let target = normalise_symbol(&s[open + 2..open + close]);
            let head = &s[..open];
            // Everything from the resolved comment onwards is layout.
            let head = head.split(" #").next().unwrap_or(head);
            s = format!("{} <{target}>", strip_addresses(head));
        }
    } else {
        s = strip_addresses(s.split(" #").next().unwrap_or(&s));
    }

    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Replace anything that looks like a linked address with `ADDR`.
///
/// Five hex digits is the floor: a real immediate in this engine is a mask, a shift or a
/// small constant, and collapsing those would hide a changed constant — which is exactly the
/// kind of change this gate exists to catch.
fn strip_addresses(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let hex_start = if bytes[i..].starts_with(&['0', 'x']) { i + 2 } else { i };
        let mut j = hex_start;
        while j < bytes.len() && bytes[j].is_ascii_hexdigit() {
            j += 1;
        }
        let digits = j - hex_start;
        let boundary = start == 0 || !bytes[start - 1].is_alphanumeric();
        if digits >= 5 && boundary {
            out.push_str("ADDR");
            i = j;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Normalise a Rust symbol name.
///
/// The `Cs<hash>_` disambiguator in a v0-mangled name is derived from the crate's metadata,
/// which includes the manifest's PATH — and the baseline is built in a worktree at a
/// different path. Left alone, every Rust symbol reads as renamed and the gate reports a
/// total rewrite for a one-line change.
fn normalise_symbol(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let chars: Vec<char> = name.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == 'C' && chars.get(i + 1) == Some(&'s') {
            let mut j = i + 2;
            while j < chars.len() && chars[j].is_ascii_alphanumeric() {
                j += 1;
            }
            // A disambiguator is `Cs`, base-62 digits, then `_`.
            if j > i + 2 && chars.get(j) == Some(&'_') {
                out.push_str("Cs_");
                i = j + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Drop the alignment padding a symbol ends on.
///
/// `f1318daa`'s correction. The assembler pads a function out to the next alignment
/// boundary, so a function whose body is untouched still ends on a different run of `nop`s
/// once anything before it changes length — and counting that reported a diff on every
/// symbol downstream of a real one, which is every symbol.
fn trim_padding(body: &mut Vec<String>) {
    while body.last().is_some_and(|l| is_padding(l)) {
        body.pop();
    }
}

/// The no-op encodings a padder emits.
fn is_padding(insn: &str) -> bool {
    let mnemonic = insn.split_whitespace().next().unwrap_or("");
    matches!(mnemonic, "nop" | "nopw" | "nopl" | "int3" | "data16" | "cs")
        || insn.starts_with("xchg %ax,%ax")
}

/// Compare the two symbol sets and report.
fn report(before: &[Symbol], after: &[Symbol]) -> Outcome {
    let index = |syms: &[Symbol]| -> std::collections::HashMap<String, Vec<String>> {
        syms.iter().map(|s| (s.name.clone(), s.body.clone())).collect()
    };
    let (a, b) = (index(before), index(after));

    let mut changed = Vec::new();
    let mut added: Vec<&String> = b.keys().filter(|k| !a.contains_key(*k)).collect();
    let mut removed: Vec<&String> = a.keys().filter(|k| !b.contains_key(*k)).collect();
    for (name, body) in &a {
        if let Some(other) = b.get(name)
            && other != body
        {
            changed.push((name.clone(), body.len(), other.len()));
        }
    }
    changed.sort();
    added.sort();
    removed.sort();

    println!(
        "codegen-equiv: {} symbols before, {} after, {} common",
        a.len(),
        b.len(),
        a.keys().filter(|k| b.contains_key(*k)).count()
    );

    for name in removed.iter().take(20) {
        println!("  GONE     {}", short(name));
    }
    for name in added.iter().take(20) {
        println!("  NEW      {}", short(name));
    }
    for (name, was, now) in changed.iter().take(20) {
        println!("  CHANGED  {} ({was} -> {now} instructions)", short(name));
    }

    let total = changed.len() + added.len() + removed.len();
    if total == 0 {
        println!("codegen-equiv: every symbol is byte-identical -- the change is a rewording");
        return Outcome::Pass;
    }
    Outcome::Fail(format!(
        "{total} symbol(s) differ: {} changed, {} added, {} removed. If that is intended, this \
         change is not a pure refactor and its commit message must not say so",
        changed.len(),
        added.len(),
        removed.len()
    ))
}

/// The tail of a mangled name, which is the part a reader recognises.
fn short(name: &str) -> &str {
    if name.len() <= 90 { name } else { &name[name.len() - 90..] }
}

/// `--flag value` out of the argument list.
fn arg_value<'a>(args: &'a [&'a str], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| *a == flag).and_then(|i| args.get(i + 1)).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The target survives, the address it landed on does not. `ADDR` is kept rather than
    /// deleted so the operand COUNT is still comparable: an instruction that gained an
    /// operand must not normalise into one that did not.
    #[test]
    fn a_call_keeps_its_target_and_loses_its_address() {
        assert_eq!(
            normalise_operands("call   401000 <_RNvCs1234_3foo>"),
            "call ADDR <_RNvCs_3foo>"
        );
    }

    #[test]
    fn a_rip_relative_load_keeps_the_datum_and_loses_the_displacement() {
        assert_eq!(
            normalise_operands("mov    0x1b2c3(%rip),%rax        # 4a1b2c <TABLE>"),
            "mov ADDR(%rip),%rax <TABLE>"
        );
    }

    /// A changed CONSTANT must survive normalisation, or the gate hides the change it is
    /// most likely to be pointed at.
    #[test]
    fn a_small_immediate_is_not_an_address() {
        assert_eq!(normalise_operands("add    $0x40,%rsp"), "add $0x40,%rsp");
        assert_eq!(normalise_operands("cmp    $0x7fff,%eax"), "cmp $0x7fff,%eax");
    }

    #[test]
    fn the_crate_disambiguator_is_normalised_away() {
        assert_eq!(normalise_symbol("_RNvMNtNtCs4nWZRYSJl3l_12rfish_engine3foo"), {
            "_RNvMNtNtCs_12rfish_engine3foo"
        });
    }

    #[test]
    fn trailing_alignment_padding_is_not_codegen() {
        let mut body = vec!["ret".to_string(), "nopw 0x0(%rax,%rax,1)".to_string()];
        trim_padding(&mut body);
        assert_eq!(body, vec!["ret".to_string()]);
    }

    /// A symbol whose whole body is padding must not become empty-and-equal to another one.
    #[test]
    fn padding_is_only_trimmed_from_the_end() {
        let mut body = vec!["nop".to_string(), "ret".to_string()];
        trim_padding(&mut body);
        assert_eq!(body.len(), 2);
    }

    #[test]
    fn a_symbol_header_is_recognised_and_an_instruction_is_not() {
        assert_eq!(
            symbol_header("0000000000401234 <_RNvCs9_3foo>:").as_deref(),
            Some("_RNvCs_3foo")
        );
        assert_eq!(symbol_header("  401234:\tret"), None);
        assert_eq!(instruction("  401234:\tret").as_deref(), Some("ret"));
    }
}
