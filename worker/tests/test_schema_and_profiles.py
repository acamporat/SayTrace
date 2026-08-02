from __future__ import annotations

import pytest

from local_transcript_worker.errors import ErrorCode, WorkerError
from local_transcript_worker.profiles import MatchPolicy, VoiceProfile, match_speaker
from local_transcript_worker.schema import Request


def test_request_rejects_incompatible_protocol() -> None:
    with pytest.raises(WorkerError) as caught:
        Request.from_dict(
            {
                "protocol_version": "2.0",
                "type": "request",
                "request_id": "request-1",
                "command": "ping",
                "payload": {},
            }
        )

    assert caught.value.code is ErrorCode.PROTOCOL_MISMATCH


def test_request_rejects_unsafe_identifier() -> None:
    with pytest.raises(WorkerError) as caught:
        Request.from_dict(
            {
                "protocol_version": "1.0",
                "type": "request",
                "request_id": "../escape",
                "command": "ping",
                "payload": {},
            }
        )

    assert caught.value.code is ErrorCode.BAD_REQUEST


def _profile(
    profile_id: str,
    name: str,
    vector: list[float],
    *,
    samples: int = 1,
    duration_ms: int = 10_000,
    confirmed: bool = True,
) -> VoiceProfile:
    return VoiceProfile.from_dict(
        {
            "profile_id": profile_id,
            "name": name,
            "embeddings": [vector] * samples,
            "sample_durations_ms": [duration_ms] * samples,
            "explicitly_confirmed": confirmed,
        }
    )


def test_matching_requires_eligible_profile_and_margin() -> None:
    policy = MatchPolicy("benchmark-1", 0.9, 0.08, 0.7)
    alex = _profile("alex", "Alex", [1.0, 0.0])
    similar = _profile("similar", "Similar", [0.99, 0.1])

    result = match_speaker([1.0, 0.0], [alex, similar], policy)

    assert result.state == "Review"
    assert result.profile_id == "alex"


def test_matching_accepts_a_confirmed_ten_second_profile() -> None:
    policy = MatchPolicy("benchmark-1", 0.9, 0.08, 0.7)
    eligible = _profile("alex", "Alex", [1.0, 0.0])
    unconfirmed = _profile("other", "Other", [1.0, 0.0], confirmed=False)

    result = match_speaker([1.0, 0.0], [eligible, unconfirmed], policy)

    assert result.state == "Matched"
    assert result.name == "Alex"


def test_ineligible_profiles_remain_unknown() -> None:
    policy = MatchPolicy("benchmark-1", 0.9, 0.08, 0.7)
    too_short = _profile("alex", "Alex", [1.0, 0.0], duration_ms=9_999)

    result = match_speaker([1.0, 0.0], [too_short], policy)

    assert result.state == "Unknown"
    assert result.profile_id is None
