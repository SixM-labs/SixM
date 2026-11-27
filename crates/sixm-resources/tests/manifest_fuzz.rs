//! Deterministic fuzzing of the `fxmanifest.lua` parser. A seeded generator
//! builds manifest-like Lua from directive names, literals and punctuation,
//! then mutates it. Each source must parse or fail with an error, never panic.

use sixm_resources::parse_manifest;

/// Sources per run. Each one creates a fresh Lua state, so the count is lower
/// than the frame decoders' 100,000.
const CASES: usize = 5_000;

const PIECES: &[&str] = &[
    "fx_version",
    "game",
    "server_script",
    "client_script",
    "shared_script",
    "file",
    "dependency",
    "dependencies",
    "exports",
    "server_exports",
    "lua54",
    "author",
    "description",
    "version",
    "files",
    "client_scripts",
    "'a.lua'",
    "\"b.lua\"",
    "'*.lua'",
    "'yes'",
    "'no'",
    "1.5",
    "-7",
    "1e308",
    "nil",
    "true",
    "{",
    "}",
    "(",
    ")",
    ",",
    "=",
    "..",
    "_G",
    "setmetatable",
    "string.rep",
    "for i = 1, 3 do end",
    "return",
    "\n",
    "--",
    "[[x]]",
];

/// Seeded xorshift64 generator.
struct Xorshift(u64);

impl Xorshift {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A number in `0..n`, or 0 when `n` is 0.
    fn below(&mut self, n: usize) -> usize {
        match u64::try_from(n) {
            Ok(n) if n > 0 => usize::try_from(self.next_u64() % n).unwrap_or(0),
            _ => 0,
        }
    }
}

const DIRECTIVES: &[&str] = &[
    "fx_version",
    "game",
    "server_script",
    "client_script",
    "shared_script",
    "file",
    "dependency",
    "dependencies",
    "export",
    "exports",
    "server_export",
    "server_exports",
    "lua54",
    "author",
    "version",
    "files",
    "client_scripts",
];

/// A random argument for a directive: text, a number, a table or a flag.
fn argument(rng: &mut Xorshift) -> String {
    match rng.below(5) {
        0 => format!("'{}.lua'", PIECES[rng.below(PIECES.len())]),
        1 => format!("\"{}\"", PIECES[rng.below(PIECES.len())]),
        2 => format!("{{ 'a.lua', '{}' }}", PIECES[rng.below(PIECES.len())]),
        3 => format!("{}", rng.below(1000)),
        _ => "'yes'".to_owned(),
    }
}

/// Random manifest-like source: directive lines with random arguments, plus
/// some loose pieces that often break the Lua syntax.
fn generate(rng: &mut Xorshift) -> String {
    let count = 1 + rng.below(12);
    let mut source = String::new();
    for _ in 0..count {
        if rng.below(6) == 0 {
            source.push_str(PIECES[rng.below(PIECES.len())]);
        } else {
            let name = DIRECTIVES[rng.below(DIRECTIVES.len())];
            let arg = argument(rng);
            if rng.below(2) == 0 {
                source.push_str(name);
                source.push(' ');
                source.push_str(&arg);
            } else {
                source.push_str(name);
                source.push('(');
                source.push_str(&arg);
                source.push(')');
            }
        }
        source.push('\n');
    }
    source
}

/// Replaces a random range of `source` with bytes or pieces, keeping it valid
/// UTF-8 by working on characters.
fn mutate(rng: &mut Xorshift, source: &str) -> String {
    let mut chars: Vec<char> = source.chars().collect();
    for _ in 0..=rng.below(3) {
        let at = rng.below(chars.len() + 1);
        match rng.below(3) {
            0 => {
                let end = (at + rng.below(8)).min(chars.len());
                chars.drain(at..end);
            }
            1 => {
                let piece = PIECES[rng.below(PIECES.len())];
                let inserted: Vec<char> = piece.chars().collect();
                chars.splice(at..at, inserted);
            }
            _ => {
                let byte = u8::try_from(rng.below(128)).unwrap_or(b'?');
                chars.insert(at, char::from(byte));
            }
        }
    }
    chars.into_iter().collect()
}

#[test]
fn five_thousand_generated_manifests_never_panic() {
    let mut rng = Xorshift(0x0fa5_75ee_d000_0001);
    let mut parsed = 0;
    for case in 0..CASES {
        let source = if case % 2 == 0 {
            generate(&mut rng)
        } else {
            let base = generate(&mut rng);
            mutate(&mut rng, &base)
        };
        if parse_manifest(&source, "@fuzz/fxmanifest.lua").is_ok() {
            parsed += 1;
        }
    }
    // A fair share of the generated sources are valid manifests.
    assert!(parsed > CASES / 10, "only {parsed} manifests parsed");
}
