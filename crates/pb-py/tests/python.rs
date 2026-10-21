//! Builds the `protobench` Python module (this crate's cdylib), puts it on
//! PYTHONPATH and runs `tests/test_protobench.py` with the local Python.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The extension module cargo built for this test run.
fn built_module() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    // target/<profile>/deps/python-<hash> → target/<profile>
    let dir = exe.parent().and_then(Path::parent).unwrap();
    for name in ["libprotobench.so", "libprotobench.dylib", "protobench.dll"] {
        let p = dir.join(name);
        if p.exists() {
            return p;
        }
    }
    panic!("no protobench library in {}", dir.display());
}

fn python() -> &'static str {
    for p in ["python3", "python"] {
        if Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return p;
        }
    }
    panic!("no python3 on PATH");
}

#[test]
fn python_module_drives_the_demo() {
    let tmp = std::env::temp_dir().join(format!("pb-py-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    // Python imports `protobench.pyd` on Windows, `protobench.abi3.so` elsewhere.
    let dest = tmp.join(if cfg!(windows) {
        "protobench.pyd"
    } else {
        "protobench.abi3.so"
    });
    std::fs::copy(built_module(), &dest).unwrap();
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let demo = here.join("../../examples/demo-sensor");
    let fixture = here.join("../pb-core/tests/fixtures/round3-demo.pbsession");
    let out = Command::new(python())
        .arg(here.join("tests/test_protobench.py"))
        .arg(&demo)
        .arg(&tmp)
        .env("PYTHONPATH", &tmp)
        .env("PB_TEST_RECORDING", &fixture)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("python tests passed"), "{text}");
    // The demo script runs too.
    let out = Command::new(python())
        .arg(demo.join("python/smoke.py"))
        .arg(&demo)
        .env("PYTHONPATH", &tmp)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("PASSED"), "{text}");
    let _ = std::fs::remove_dir_all(tmp);
}
