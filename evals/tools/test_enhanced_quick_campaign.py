import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


MODULE_PATH = Path(__file__).with_name("enhanced_quick_campaign.py")
SPEC = importlib.util.spec_from_file_location("enhanced_quick_campaign", MODULE_PATH)
CAMPAIGN = importlib.util.module_from_spec(SPEC)
assert SPEC.loader
SPEC.loader.exec_module(CAMPAIGN)


def result(task, model, profile, correct=True, event=None, tokens=100, duration=1000):
    events = {event: 1} if event else {}
    return {
        "caseId": f"{task}--{model}--r1",
        "taskId": task,
        "model": model,
        "repetition": 1,
        "passed": correct,
        "taskCorrectnessPassed": correct,
        "agentCompleted": correct,
        "protocolQualified": True,
        "performanceQualified": True,
        "protocolViolations": [],
        "harnessExpectationMisses": [],
        "durationMs": duration,
        "layers": {"runtimeAttribution": True, "mechanismExercised": True},
        "metrics": {
            "inputTokens": tokens,
            "outputTokens": 0,
            "enhancedRuntime": {"featureProfile": profile},
            "enhancedEventCounts": events,
        },
    }


class ScoreTests(unittest.TestCase):
    def test_requires_mechanism_event_and_preserves_controls(self):
        target = CAMPAIGN.CONFIG["E1"]["target"]
        event = CAMPAIGN.CONFIG["E1"]["event"]
        baseline, candidate = [], []
        for model in ("qwen", "omen"):
            for task in (target, "short", "multi"):
                baseline.append(result(task, model, "E0", correct=task != target))
                candidate.append(result(task, model, "E1", event=event if task == target else None))
        with patch.object(CAMPAIGN, "read_results", side_effect=[baseline, candidate]):
            report = CAMPAIGN.score("E1", "base", "candidate")
        self.assertTrue(report["passed"], report["failures"])
        self.assertEqual(report["targetMechanismEventCount"], 2)

    def test_rejects_missing_mechanism_event(self):
        target = CAMPAIGN.CONFIG["E2"]["target"]
        baseline, candidate = [], []
        for model in ("qwen", "omen"):
            for task in (target, "short", "multi"):
                baseline.append(result(task, model, "E0"))
                candidate.append(result(task, model, "E2"))
        with patch.object(CAMPAIGN, "read_results", side_effect=[baseline, candidate]):
            report = CAMPAIGN.score("E2", "base", "candidate")
        self.assertFalse(report["passed"])
        self.assertTrue(any("did not emit" in failure for failure in report["failures"]))

    def test_correctness_alone_cannot_bypass_protocol_qualification(self):
        target = CAMPAIGN.CONFIG["E3"]["target"]
        event = CAMPAIGN.CONFIG["E3"]["event"]
        baseline, candidate = [], []
        for model in ("qwen", "omen"):
            for task in (target, "short", "multi"):
                baseline.append(result(task, model, "E0", correct=task != target))
                item = result(task, model, "E3", event=event if task == target else None)
                if task == target:
                    item["protocolQualified"] = False
                candidate.append(item)
        with patch.object(CAMPAIGN, "read_results", side_effect=[baseline, candidate]):
            report = CAMPAIGN.score("E3", "base", "candidate")
        self.assertFalse(report["passed"])
        self.assertTrue(any("protocol qualification failed" in failure for failure in report["failures"]))


if __name__ == "__main__":
    unittest.main()
