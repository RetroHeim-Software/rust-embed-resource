use self::super::ParameterBundle;
use std::path::{Path, PathBuf, MAIN_SEPARATOR};
use std::process::Command;
use std::borrow::Cow;
use std::ffi::OsStr;
use std::env;


#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResourceCompiler;


impl ResourceCompiler {
    #[inline(always)]
    pub fn new() -> ResourceCompiler {
        ResourceCompiler
    }

    #[inline(always)]
    pub fn is_supported(&mut self) -> Option<Cow<'static, str>> {
        None
    }

    pub fn compile_resource<Ms: AsRef<OsStr>, Mi: IntoIterator<Item = Ms>, Is: AsRef<OsStr>, Ii: IntoIterator<Item = Is>>(
        &self, out_dir: &str, prefix: &str, resource: &str, parameters: ParameterBundle<Ms, Mi, Is, Ii>)
        -> Result<String, Cow<'static, str>> {
        let out_file = format!("{}{}lib{}.a", out_dir, MAIN_SEPARATOR, prefix);

        // Under some msys2 environments, $MINGW_CHOST has the correct target for
        // GNU windres or llvm-windres (clang32, clang64, or clangarm64)
        let target = env::var_os("MINGW_CHOST").map(Cow::Owned).unwrap_or_else(|| {
            OsStr::new(match env::var_os("TARGET").expect("No TARGET env var").as_encoded_bytes() {
                    [b'x', b'8', b'6', b'_', b'6', b'4', ..] => "pe-x86-64", // "x86_64"
                    [b'a', b'a', b'r', b'c', b'h', b'6', b'4', ..] => "pe-aarch64-little", // "aarch64"
                    // windres has "pe-aarch64-little" in the strings but doesn't actually accept it on my machine,
                    // llvm-windres only has i686 and amd64; still unported
                    _ => "pe-i386",
                })
                .into()
        });

        // GNU windres builds its own preprocessor (cc1) invocation internally
        // by joining its `--input`/`-I`/`-D` arguments into a bare string and
        // re-tokenizing it on whitespace, instead of using a real argv array
        // -- so any argument containing a space breaks it, even though *this*
        // process passes windres its own arguments correctly (one argv entry
        // each, verified by direct repro: `windres --input "C:\...\resource.rc"
        // ...` on a path with a space fails with a separate "cc1.exe: fatal
        // error: <fragment>: No such file or directory" for every
        // space-delimited fragment of the path). `--use-temp-file` does not
        // help -- the broken re-tokenization happens before that option's
        // effect applies. Side-step it by running windres with its working
        // directory set to the deepest common ancestor of `resource`/
        // `out_file`/every include dir, and referring to each by its path
        // relative to that ancestor, so windres's internal command string
        // never contains a space regardless of where the crate is checked
        // out or where its include dirs live relative to `out_dir`.
        // Absolutize every caller-supplied include dir against the
        // pre-`current_dir` working directory (the crate-root context the
        // caller actually meant a relative dir like `"src/include"` to
        // resolve against) -- `apply_parameters` would otherwise pass a
        // relative dir through unchanged, and it would silently resolve
        // against `out_dir` once `.current_dir()` below takes effect.
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let include_dir_paths: Vec<PathBuf> = parameters
            .include_dirs
            .into_iter()
            .map(|id| {
                let id = id.as_ref().to_string_lossy().into_owned();
                let p = PathBuf::from(&id);
                if p.is_absolute() { p } else { cwd.join(&id) }
            })
            .collect();

        // An include dir is the normal case for a real crate (e.g.
        // `src/include` at the crate root) that lives OUTSIDE `out_dir_path`
        // -- `relative_to(out_dir_path, ...)` can't shorten it, so it would
        // pass through as a full absolute path, reintroducing the exact
        // whitespace re-tokenization bug this file exists to avoid on a
        // checkout path with a space. Fix: use the deepest directory that
        // actually contains resource/out_file/every include dir as the
        // command's single working directory, and relativize every argument
        // against THAT common ancestor instead of always assuming
        // `out_dir_path` is deep enough to contain everything. `resource`/
        // `out_file` can themselves be relative (same as the include dirs
        // above), so absolutize them against `cwd` too before comparing --
        // `common_ancestor` requires both sides to already be absolute.
        let absolutize = |p: &str| -> PathBuf {
            let p = Path::new(p);
            if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) }
        };
        let resource_path = absolutize(resource);
        let out_file_path = absolutize(&out_file);
        let out_dir_abs = absolutize(out_dir);
        let mut common = out_dir_abs.clone();
        for p in std::iter::once(&resource_path)
            .chain(std::iter::once(&out_file_path))
            .chain(include_dir_paths.iter())
        {
            common = common_ancestor(&common, p);
        }
        // Paths on different Windows volumes (`C:\...` vs `D:\...`, or UNC
        // vs drive) share no component at all, so `common_ancestor`
        // correctly yields an empty path -- but handing that to
        // `current_dir` is a broken working directory and windres never
        // even starts, where the unpatched crate (absolute args, no
        // `current_dir`) succeeded. There is no shortening to be had in
        // that case, so fall back to the process's own working directory:
        // arguments under it still shorten, the rest stay absolute
        // (i.e. upstream's behaviour, which is the best available here --
        // such a layout cannot be made space-free by any single working
        // directory anyway).
        if common.components().next().is_none() {
            common = cwd.clone();
        }

        let resource_str = resource_path.to_string_lossy().into_owned();
        let out_file_str = out_file_path.to_string_lossy().into_owned();
        let out_dir_str = out_dir_abs.to_string_lossy().into_owned();
        let resource_arg = relative_to(&common, &resource_str);
        let out_file_arg = relative_to(&common, &out_file_str);
        let include_dir_args: Vec<String> = include_dir_paths
            .iter()
            .map(|p| relative_to(&common, &p.to_string_lossy()).into_owned())
            .collect();
        let out_dir_arg = relative_to(&common, &out_dir_str);

        let mut cmd = Command::new("windres");
        cmd.current_dir(&common)
            .args(&["--input", &*resource_arg, "--output-format=coff", "--target"])
            .arg(target)
            .args(&["-c", "65001"]) // UTF-8, cf. https://github.com/nabijaczleweli/rust-embed-resource/pull/73
            .args(&["--output", &*out_file_arg, "--include-dir", &*out_dir_arg]);
        for id in &include_dir_args {
            cmd.arg("-I").arg(id);
        }
        for m in parameters.macros {
            cmd.arg("-D").arg(m);
        }

        match cmd.status() {
            Ok(stat) if stat.success() => Ok(out_file),
            Ok(stat) => Err(format!("windres failed to compile \"{}\" into \"{}\" with {}", resource, out_file, stat).into()),
            Err(e) => Err(format!("Couldn't to execute windres to compile \"{}\" into \"{}\": {}", resource, out_file, e).into()),
        }
    }
}


pub fn find_windows_sdk_tool_impl(_: &str) -> Option<PathBuf> {
    None
}


/// `path`, relative to `base` when it's nested under `base`; otherwise `path`
/// unchanged. Used to keep windres's own argument list space-free (see the
/// call site comment in `compile_resource`).
///
/// A path EQUAL to `base` relativizes to `"."`, never `""`: in the standard
/// tauri-winres invocation (resource, out_file, include dirs all under
/// `OUT_DIR`) the common ancestor IS `out_dir`, so `out_dir`'s own argument
/// hits this case -- and an empty string passed to windres can make it
/// misinterpret argument boundaries rather than being harmlessly ignored
/// (same hazard class the ocio windres shim documents). `"."` is exactly
/// what the Windows-verified pre-rework code passed for `--include-dir`.
fn relative_to<'p>(base: &Path, path: &'p str) -> Cow<'p, str> {
    match Path::new(path).strip_prefix(base).ok().and_then(|rel| rel.to_str()) {
        Some("") => Cow::Borrowed("."),
        Some(rel) => Cow::Owned(rel.to_owned()),
        None => Cow::Borrowed(path),
    }
}

/// The deepest directory that is an ancestor of (or equal to) both `a` and
/// `b`, compared component-wise. Both inputs must be absolute -- callers
/// (`compile_resource`) guarantee this by absolutizing every path against
/// the working directory before this is ever called. Used to find a single
/// `current_dir` every resource/output/include-dir argument can be
/// relativized against, even when they don't all live under `out_dir`.
///
/// Paths that share NO components (different Windows drive letters, or a UNC
/// share vs a drive path) produce an empty `PathBuf`. `compile_resource`
/// guards that case (falling back to the process cwd) -- an empty path
/// handed to `Command::current_dir` would fail the spawn outright, where the
/// unpatched crate (absolute args, no `current_dir`) succeeded.
fn common_ancestor(a: &Path, b: &Path) -> PathBuf {
    let a_components: Vec<_> = a.components().collect();
    let b_components: Vec<_> = b.components().collect();
    let shared = a_components
        .iter()
        .zip(b_components.iter())
        .take_while(|(x, y)| x == y)
        .count();
    a_components[..shared].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::{common_ancestor, relative_to};
    use std::path::{Path, PathBuf};

    // These run only when the test target is windows-gnu (this module is
    // cfg-gated), but they are host-shape-independent: `relative_to` and
    // `common_ancestor` are pure path math.

    #[test]
    fn relative_to_a_path_equal_to_base_is_dot_not_empty() {
        // The standard tauri-winres case: out_dir relativized against a
        // common ancestor that IS out_dir. An empty string here regressed
        // the verified `--include-dir "."` behavior.
        let base = Path::new(r"C:\build\out");
        assert_eq!(relative_to(base, r"C:\build\out"), ".");
    }

    #[test]
    fn relative_to_strips_a_nested_prefix() {
        let base = Path::new(r"C:\build\out");
        assert_eq!(relative_to(base, r"C:\build\out\resource.rc"), "resource.rc");
    }

    #[test]
    fn relative_to_shortens_a_nested_path_and_passes_an_unrelated_one_through() {
        assert_eq!(&*relative_to(Path::new(r"C:\a b"), r"C:\a b\out\libfoo.a"), r"out\libfoo.a");
        assert_eq!(&*relative_to(Path::new(r"C:\a b"), r"D:\src\resource.rc"), r"D:\src\resource.rc");
    }

    #[test]
    fn common_ancestor_of_same_volume_paths_is_the_deepest_shared_directory() {
        assert_eq!(common_ancestor(Path::new(r"C:\a b\out\build"), Path::new(r"C:\a b\src\resource.rc")),
                   PathBuf::from(r"C:\a b"));
    }

    #[test]
    fn common_ancestor_of_a_path_and_its_own_ancestor_is_that_ancestor() {
        assert_eq!(common_ancestor(Path::new(r"C:\a b"), Path::new(r"C:\a b\out\libfoo.a")), PathBuf::from(r"C:\a b"));
    }

    #[test]
    fn common_ancestor_of_paths_sharing_only_the_volume_root_is_that_root() {
        assert_eq!(common_ancestor(Path::new(r"C:\out"), Path::new(r"C:\src")), PathBuf::from(r"C:\"));
    }

    #[test]
    fn common_ancestor_across_volumes_is_empty_and_must_be_guarded() {
        assert_eq!(common_ancestor(Path::new(r"C:\out"), Path::new(r"D:\src")), PathBuf::new());
    }
}
