"""Golden fixtures from Splash 1.4.1's own protocol.py, for crates/splash/tests/engine_protocol.rs.

Run with Homebrew Splash's interpreter (it reads nothing but this folder and the reference):
  /opt/homebrew/Cellar/splash/1.4.1/libexec/python/bin/python3 -I generate.py

Writes one `<name>.hex` per frame (client frames by serialize_message; engine events packed by hand per the struct
formats, then checked by decode_frame) and `validation_expected.json`: each case of `validation_cases.json` as the
reference serializes or refuses it.
"""

import json
import struct
import sys
from pathlib import Path

sys.path.insert(0, "/opt/homebrew/Cellar/splash/1.4.1/libexec")
from server import protocol as p  # noqa: E402

HERE = Path(__file__).resolve().parent


def write(name, data):
    (HERE / f"{name}.hex").write_text(bytes(data).hex() + "\n")


def event(frame_type, payload):
    decoded = p.decode_frame(p.Frame(frame_type, payload))  # the reference must accept it
    frame = struct.pack("<4sHHQ", b"SPLH", p.PROTOCOL_VERSION, int(frame_type), len(payload)) + payload
    return frame, decoded


# ---- client frames ----
pixels = bytes((i * 7) % 256 for i in range(2 * 16 * 4 * 16 * 3))
full = p.RequestFrame(
    request_id=0x0102030405060708,
    priority=p.RequestPriority.FOREGROUND,
    absolute_deadline_unix_micros=1_761_000_000_000_000,
    remaining_deadline_micros=30_000_000,
    logical_max_output_tokens=256,
    prompt_tokens=(100, 101, 9, 9, 102, 103, 104, 105),
    sampling=p.SamplingParameters(0.7, 0.95, 40, 0.5, -0.25, 1.05, 0.05),
    seed=0xDEADBEEFCAFEF00D,
    constraint=p.ConstraintMode.TOKEN_MASK,
    image_spans=(p.ImageSpan(2, 2, 2, 4, 0x1122334455667788, 0x99AABBCCDDEEFF00),),
    image_pixels=pixels,
    score_tokens=(),
    generation_prompt_tokens=2,
    flags=p.RequestFlag.RETURN_PROGRESS,
    cache_salt=0x0123456789ABCDEF_FEDCBA9876543210,
    thinking_budget=p.ThinkingBudget(512, (200, 201), 202),
)
write("request_full", p.serialize_message(full))
score = p.RequestFrame(
    request_id=9,
    priority=p.RequestPriority.BACKGROUND,
    absolute_deadline_unix_micros=(1 << 64) - 1,
    remaining_deadline_micros=(1 << 64) - 1 - 1_761_000_000_000_000,
    logical_max_output_tokens=0,
    prompt_tokens=(1, 2, 3),
    sampling=p.SamplingParameters(),
    seed=0,
    constraint=p.ConstraintMode.NONE,
    image_spans=(),
    image_pixels=b"",
    score_tokens=(10, 20, 30),
    generation_prompt_tokens=0,
    flags=0,
    cache_salt=0,
)
write("request_score", p.serialize_message(score))
write("cancel", p.serialize_message(p.CancelFrame(7)))
write(
    "mask_response",
    p.serialize_message(p.MaskResponseFrame(7, 3, struct.pack("<4I", 0xFFFFFFFF, 1, 0x80000000, 0))),
)
write("status_request", p.serialize_message(p.StatusRequestFrame(42)))
write("release_request", p.serialize_message(p.ReleaseRequestFrame(43)))

# ---- engine events ----
T = p.FrameType
events = {
    "ready": (T.READY, struct.pack("<IIB", 4, 32768, 1)),
    "start": (T.START, struct.pack("<QII", 7, 2, 128)),
    "prompt_progress": (T.PROMPT_PROGRESS, struct.pack("<QIQ", 7, 64, 1500)),
    "tokens": (T.TOKENS, struct.pack("<QII3I", 7, 3, 3, 100, 101, 102)),
    "mask_request": (T.MASK_REQUEST, struct.pack("<QQIII", 7, 3, 2, 1, 100)),
    "done_length": (T.DONE, struct.pack("<QBBIIQQQIIIII", 7, 1, 0, 8, 3, 1000, 2000, 3500, 4, 6, 4, 2, 0)),
    "done_bounded": (T.DONE, struct.pack("<QBBIIQQQIIIII", 7, 1, 1, 8, 2, 1, 2, 3, 0, 0, 0, 0, 0)),
    "done_score": (
        T.DONE,
        struct.pack("<QBBIIQQQIIIII", 9, 0, 0, 3, 0, 10, 0, 20, 0, 0, 0, 0, 3) + struct.pack("<3f", 1.5, -2.25, 0.0),
    ),
    "error_request": (
        T.ERROR,
        struct.pack("<BBIQII", 1, 1, 5, 7, 13, 21) + b"kv_disk_quota" + b"the SSD cache is full",
    ),
    "error_engine": (T.ERROR, struct.pack("<BBIQII", 2, 0, 0, 0, 13, 9) + b"metal_failure" + b"GPU fault"),
    "status_json": (T.STATUS_JSON, struct.pack("<Q", 42) + b'{"schema_version":6,"ready":true}'),
}
for name, (frame_type, payload) in events.items():
    frame, _ = event(frame_type, payload)
    write(name, frame)


# ---- validation cases ----
def number(value):
    return float(value) if isinstance(value, str) else value


def request_from(case):
    prompt = case.get("prompt_tokens", [1, 2, 3, 4, 5, 6])
    if isinstance(prompt, dict):
        prompt = [7] * prompt["repeat"]
    sampling = {k: number(v) for k, v in case.get("sampling", {}).items()}
    thinking = case.get("thinking_budget")
    salt = case.get("cache_salt", 0)
    return p.RequestFrame(
        request_id=case.get("request_id", 7),
        priority=p.RequestPriority(case.get("priority", 1)),
        absolute_deadline_unix_micros=case.get("absolute", 1000),
        remaining_deadline_micros=case.get("remaining", 500),
        logical_max_output_tokens=case.get("logical_max_output_tokens", 16),
        prompt_tokens=tuple(prompt),
        sampling=p.SamplingParameters(**sampling),
        seed=case.get("seed", 0),
        constraint=p.ConstraintMode(case.get("constraint", 0)),
        image_spans=tuple(p.ImageSpan(*s) for s in case.get("image_spans", [])),
        image_pixels=bytes(case.get("image_pixels", 0)),
        score_tokens=tuple(case.get("score_tokens", [])),
        generation_prompt_tokens=case.get("generation_prompt_tokens", 0),
        flags=case.get("flags", 0),
        cache_salt=int(salt),
        thinking_budget=None
        if thinking is None
        else p.ThinkingBudget(thinking["tokens"], tuple(thinking["end"]), thinking["also_ends"]),
    )


expected = {}
for case in json.loads((HERE / "validation_cases.json").read_text()):
    try:
        frame = p.serialize_message(request_from(case))
        expected[case["name"]] = {"ok": len(frame) < 65536 and bytes(frame).hex() or len(frame)}
    except p.ProtocolError as error:
        issue = error.issue
        expected[case["name"]] = {
            "class": p.failure_class_name(issue.failure_class),
            "code": p.issue_code_name(issue.code),
            "request_id": issue.request_id,
            "message": issue.message,
        }
(HERE / "validation_expected.json").write_text(json.dumps(expected, indent=1) + "\n")
print(f"{len(events) + 6} frames, {len(expected)} validation cases")
