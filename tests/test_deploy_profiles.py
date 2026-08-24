from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def service_name(path: str) -> str:
    """Read metadata.name without adding a YAML dependency to the test suite."""
    lines = (ROOT / path).read_text(encoding="utf-8").splitlines()
    metadata = lines.index("metadata:")
    for line in lines[metadata + 1 :]:
        if line and not line.startswith(" "):
            break
        if line.startswith("  name: "):
            return line.split(":", 1)[1].strip()
    raise AssertionError(f"metadata.name missing from {path}")


def test_t1_full_rollback_targets_the_live_demo_service() -> None:
    assert service_name("deploy/cloudrun-demo.yaml") == "rgbmvp-demo"
    assert service_name("deploy/cloudrun-demo-freeze.yaml") == "rgbmvp-demo"
    assert service_name("deploy/cloudrun.yaml") == "rgbmvp-public"


def test_t1_demo_uses_explicit_google_xff_suffix() -> None:
    demo = (ROOT / "deploy/cloudrun-demo.yaml").read_text(encoding="utf-8")
    assert "- name: LABD_XFF_TRUSTED_HOPS\n              value: \"1\"" in demo
    assert "- name: LABD_TRUST_XFF" not in demo


def test_t1_demo_pins_turnstile_action_and_hostname_context() -> None:
    demo = (ROOT / "deploy/cloudrun-demo.yaml").read_text(encoding="utf-8")
    swap = (ROOT / "web/swap.html").read_text(encoding="utf-8")
    server = (ROOT / "crates/lab-cli/src/demo_swap.rs").read_text(encoding="utf-8")
    assert "- name: LABD_DEMO_TURNSTILE_HOSTNAMES" in demo
    assert 'action: "rgbmvp_demo_swap"' in swap
    assert 'pub const TURNSTILE_ACTION: &str = "rgbmvp_demo_swap";' in server


def test_t1_freeze_profile_removes_mutation_and_custody() -> None:
    freeze = (ROOT / "deploy/cloudrun-demo-freeze.yaml").read_text(encoding="utf-8")
    forbidden = (
        "- name: LABD_DEMO_SWAPS",
        "- name: LABD_API_TOKEN",
        "- name: RGBMVP_SECRET_DIR",
        "- name: LABD_DEMO_TURNSTILE_SECRET",
        "- name: LABD_DEMO_TURNSTILE_HOSTNAMES",
        "- name: LABD_TRUST_XFF",
        "- name: LABD_DEMO_SWEEP_INTERVAL_SECS",
        "secretKeyRef:",
        "volumeMounts:",
        "\n      volumes:",
        "serviceAccountName: rgbmvp-demo-run@",
    )
    for value in forbidden:
        assert value not in freeze
    assert (
        "serviceAccountName: rgbmvp-public-run@PROJECT.iam.gserviceaccount.com"
        in freeze
    )
