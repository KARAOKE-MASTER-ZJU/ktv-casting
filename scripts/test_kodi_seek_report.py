import datetime as dt
import json
import unittest
from kodi_seek_report import report


class ReportTests(unittest.TestCase):
    def test_failed_command_and_other_target_do_not_inherit_resume(self):
        stamp = dt.datetime(2026, 9, 17, 11, 12, 28).astimezone().isoformat()
        command = {"event": "seek_command", "target": 75, "success": False,
                   "elapsed_ms": 0, "wall_time": stamp}
        lines = "\n".join([
            "2026-09-17 11:12:28.020 T:1 debug <general>: demuxer seek to: 75000.000000",
            "2026-09-17 11:12:28.030 T:1 debug <general>: demuxer seek to: 185000.000000",
            "2026-09-17 11:12:28.650 T:1 debug <general>: CDVDAudio::Resume - resume audio stream",
        ])
        self.assertNotIn("audio_resume_log_ms", report(json.dumps(command), lines)["rows"][0])
        command["success"] = True
        self.assertNotIn("audio_resume_log_ms", report(json.dumps(command), lines)["rows"][0])

    def test_milestones_and_missing_recovery_do_not_claim_av_success(self):
        stamp = dt.datetime(2026, 9, 17, 11, 12, 28).astimezone()
        commands = [
            {"event": "seek_command", "target": 75, "success": True, "elapsed_ms": 2,
             "wall_time": (stamp + dt.timedelta(milliseconds=2)).isoformat()},
            {"event": "seek_observation", "source_requests": 0},
            {"event": "seek_command", "target": 185, "success": True, "elapsed_ms": 0,
             "wall_time": (stamp + dt.timedelta(seconds=2)).isoformat()},
        ]
        logs = "\n".join([
            "2026-09-17 11:12:28.010 T:1 debug <general>: CDVDAudio::Resume - resume audio stream",
            "2026-09-17 11:12:28.020 T:1 debug <general>: demuxer seek to: 75000.000000",
            "2026-09-17 11:12:28.200 T:1 debug <general>: demuxer seek to: 75000.000000, success",
            "2026-09-17 11:12:28.600 T:1 debug <general>: CVideoPlayerVideo - CDVDMsg::GENERAL_RESYNC(75000000)",
            "2026-09-17 11:12:28.650 T:1 debug <general>: CDVDAudio::Resume - resume audio stream",
        ])
        result = report("\n".join(map(json.dumps, commands)), logs)
        self.assertEqual(result["rows"][0]["audio_resume_log_ms"], 650)
        self.assertEqual(result["rows"][0]["demux_ready_ms"], 200)
        self.assertFalse(result["rows"][0]["av_recovery_verified"])
        self.assertEqual(result["audio_resume_log_summary"]["missing"], 1)
        self.assertNotIn("audio_resume_log_ms", result["rows"][1])


if __name__ == "__main__":
    unittest.main()
