//! Path resolution: examples, errors, and every short path over a small
//! alphabet against a reference model.

use linux_compat::errno::{EINVAL, ENAMETOOLONG, ENOENT};
use linux_compat::mem::PATH_MAX;
use linux_compat::path::{resolve, split_last, NAME_MAX};

fn r(cwd: &str, p: &str) -> (String, bool) {
    let mut out = [0u8; PATH_MAX];
    let res = resolve(cwd.as_bytes(), p.as_bytes(), &mut out).unwrap();
    (
        String::from_utf8(out[..res.len].to_vec()).unwrap(),
        res.dir_only,
    )
}

#[test]
fn examples() {
    assert_eq!(r("/", "a"), ("/a".into(), false));
    assert_eq!(r("/x/y", "a/b"), ("/x/y/a/b".into(), false));
    assert_eq!(r("/x/y", "/a"), ("/a".into(), false));
    assert_eq!(r("/x/y", "../z"), ("/x/z".into(), false));
    assert_eq!(r("/x/y", "./z/./w"), ("/x/y/z/w".into(), false));
    assert_eq!(r("/", "//a///b//"), ("/a/b".into(), true));
    assert_eq!(r("/", "/"), ("/".into(), true));
    assert_eq!(r("/a", ".."), ("/".into(), true));
    assert_eq!(r("/a", "a/.."), ("/a".into(), true));
    assert_eq!(r("/a", "."), ("/a".into(), true));
    assert_eq!(r("/a/b", "x/."), ("/a/b/x".into(), true));
    assert_eq!(
        r("/", "a/b/../../.."),
        ("/".into(), true),
        "the root has no parent"
    );
    assert_eq!(
        r("/", "../../../etc/passwd"),
        ("/etc/passwd".into(), false),
        "no way out of the root"
    );
    assert_eq!(r("/a/b/c", "../../../../.."), ("/".into(), true));
}

#[test]
fn errors() {
    let mut out = [0u8; PATH_MAX];
    assert_eq!(resolve(b"/", b"", &mut out), Err(ENOENT));
    assert_eq!(
        resolve(b"relative", b"a", &mut out),
        Err(EINVAL),
        "the working directory must be absolute"
    );
    assert_eq!(resolve(b"", b"a", &mut out), Err(EINVAL));
    // An absolute path does not need the working directory at all.
    assert!(resolve(b"", b"/a", &mut out).is_ok());
    let long = "a".repeat(NAME_MAX + 1);
    assert_eq!(resolve(b"/", long.as_bytes(), &mut out), Err(ENAMETOOLONG));
    let ok = "a".repeat(NAME_MAX);
    assert!(resolve(b"/", ok.as_bytes(), &mut out).is_ok());
    let huge = "/a".repeat(PATH_MAX);
    assert_eq!(resolve(b"/", huge.as_bytes(), &mut out), Err(ENAMETOOLONG));
    // Deep but legal: many short components just under the limit.
    let deep = "a/".repeat(PATH_MAX / 2 - 2);
    assert!(resolve(b"/", deep.as_bytes(), &mut out).is_ok());
    // A working directory that leaves no room for the component.
    let cwd = format!("/{}", "b".repeat(PATH_MAX - 3));
    assert_eq!(resolve(cwd.as_bytes(), b"abc", &mut out), Err(ENAMETOOLONG));
}

#[test]
fn split_last_parts() {
    assert_eq!(split_last(b"/a/b"), Some((&b"/a"[..], &b"b"[..])));
    assert_eq!(split_last(b"/a"), Some((&b"/"[..], &b"a"[..])));
    assert_eq!(split_last(b"/"), None);
    assert_eq!(split_last(b""), None);
}

/// The specification as a stack of components.
fn model(cwd: &str, path: &str) -> (String, bool) {
    let mut stack: Vec<&str> = if path.starts_with('/') {
        Vec::new()
    } else {
        cwd.split('/').filter(|c| !c.is_empty()).collect()
    };
    let mut last = "";
    for c in path.split('/').filter(|c| !c.is_empty()) {
        last = c;
        match c {
            "." => {}
            ".." => {
                stack.pop();
            }
            _ => stack.push(c),
        }
    }
    let s = if stack.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", stack.join("/"))
    };
    (s, path.ends_with('/') || last == "." || last == "..")
}

#[test]
fn every_path_over_a_small_alphabet_matches_the_model_and_stays_normal() {
    let alphabet = ['a', 'b', '.', '/'];
    let mut count = 0u64;
    for len in 1..=8usize {
        for code in 0..4u32.pow(len as u32) {
            let path: String = (0..len)
                .map(|i| alphabet[(code / 4u32.pow(i as u32) % 4) as usize])
                .collect();
            for cwd in ["/", "/a", "/a/b", "/b/a/."]
                .iter()
                .filter(|c| !c.ends_with('.'))
            {
                let (got, dir) = r(cwd, &path);
                assert_eq!(
                    (got.clone(), dir),
                    model(cwd, &path),
                    "cwd {cwd:?} path {path:?}"
                );
                // Normal form: absolute, no dots, no doubled or trailing slash.
                assert!(got.starts_with('/'));
                assert!(!got.contains("//") && !got.contains("/./") && !got.contains("/../"));
                assert!(got == "/" || !got.ends_with('/'));
                assert!(!got.ends_with("/.") && !got.ends_with("/.."));
                // Resolving a normal path again changes nothing.
                assert_eq!(r("/", &got).0, got);
                count += 1;
            }
        }
    }
    assert_eq!(count, 87_380 * 3);
}

#[test]
fn nothing_climbs_above_the_root() {
    // Whatever mixture of dots and names, the result is a path below "/": it
    // can only be the same path with the dots removed.
    for ups in 0..40 {
        let path = format!("{}etc", "../".repeat(ups));
        let (got, _) = r("/a/b/c", &path);
        assert!(got == "/etc" || got.ends_with("/etc"), "{got}");
        assert!(!got.contains(".."));
    }
}
