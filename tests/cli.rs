mod common;

use common::{Layer, Psd};
use std::process::Command;

fn psdc() -> Command {
    Command::new(env!("CARGO_BIN_EXE_psdc"))
}

fn decode(path: &std::path::Path) -> (u32, u32, Vec<u8>) {
    let mut reader = png::Decoder::new(std::fs::File::open(path).unwrap()).read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

fn sample(dir: &std::path::Path, name: &str, rgba: [u8; 4]) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, Psd::new(12, 8).layer(Layer::solid("a", 0, 0, 12, 8, rgba)).build()).unwrap();
    path
}

#[test]
fn renders_next_to_input() {
    let dir = tempfile::tempdir().unwrap();
    let input = sample(dir.path(), "page.psd", [10, 200, 30, 255]);
    let status = psdc().arg(&input).arg("-q").arg("--no-system-fonts").status().unwrap();
    assert!(status.success());
    let (w, h, px) = decode(&dir.path().join("page.png"));
    assert_eq!((w, h), (12, 8));
    assert_eq!(&px[..3], &[10, 200, 30]);
}

#[test]
fn batch_into_directory() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    let a = sample(dir.path(), "a.psd", [255, 0, 0, 255]);
    let b = sample(dir.path(), "b.psd", [0, 0, 255, 128]);
    let status =
        psdc().args([&a, &b]).arg("-o").arg(&out).args(["-q", "--no-system-fonts", "-c", "9"]).status().unwrap();
    assert!(status.success());
    assert!(out.join("a.png").exists());
    let (_, _, px) = decode(&out.join("b.png"));
    assert_eq!(&px[..4], &[0, 0, 255, 128]);
}

#[test]
fn explicit_output_file() {
    let dir = tempfile::tempdir().unwrap();
    let input = sample(dir.path(), "in.psd", [1, 2, 3, 255]);
    let out = dir.path().join("custom.png");
    assert!(psdc().arg(&input).arg("-o").arg(&out).args(["-q", "--no-system-fonts"]).status().unwrap().success());
    assert!(out.exists());
}

#[test]
fn fails_on_bad_input() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.psd");
    std::fs::write(&bad, b"not a psd").unwrap();
    let out = psdc().arg(&bad).arg("--no-system-fonts").output().unwrap();
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).is_empty());
}

#[test]
fn help_mentions_fonts() {
    let out = psdc().arg("--help").output().unwrap();
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--fonts") && help.contains("PSDC_FONTS"));
}
