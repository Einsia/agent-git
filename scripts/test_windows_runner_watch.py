import runpy
import unittest
from pathlib import Path
from unittest.mock import patch


watch = runpy.run_path(str(Path(__file__).with_name("windows-runner-watch.py")))
reconcile = watch["reconcile"]
queue = watch["QUEUE"]


class RunnerDispatch(unittest.TestCase):
    def setUp(self):
        self.now = 1000
        self.power = "running"
        self.runner = {"paused": False, "status": "online"}
        self.admitted = []
        self.pending = []
        self.running = []
        self.state = {}
        mocks = dict(gitlab=self.gitlab, ec2=self.ec2, log=lambda message: None,
                     pending_jobs=lambda: self.pending, admit=self.admitted.append)
        context = patch.dict(reconcile.__globals__, mocks)
        context.start()
        self.addCleanup(context.stop)

    def tick(self, seconds=30):
        self.now += seconds
        with patch("time.time", return_value=self.now):
            reconcile(self.state, lambda: None)

    def gitlab(self, endpoint, **update):
        if endpoint.startswith("runners/24/jobs?"):
            return self.running
        runner = {"runners/24": self.runner}[endpoint]
        runner.update(update)
        return runner

    def ec2(self, action):
        if action == "describe-instances":
            instance = {"State": {"Name": self.power}, "Tags": [{"Key": "Role", "Value": queue}]}
            return {"Reservations": [{"Instances": [instance]}]}
        self.power = {"stop-instances": "stopped", "start-instances": "running"}[action]

    def test_racing_job_prevents_idle_stop_and_queued_work_wakes_drained_runner(self):
        self.tick()
        self.tick(watch["IDLE"])
        self.assertTrue(self.runner["paused"])
        self.running.append(10)
        self.tick()
        self.assertEqual(self.power, "running")
        self.assertFalse(self.runner["paused"])
        self.running.clear()
        self.tick(watch["IDLE"])
        self.tick()
        self.assertEqual(self.power, "stopped")
        self.pending.append({"name": "windows:route", "id": 11})
        self.tick()
        self.tick()
        self.assertEqual(self.power, "running")
        self.assertFalse(self.runner["paused"])
        self.assertFalse(self.admitted)

    def test_only_fresh_runner_contact_admits_each_route_once(self):
        self.pending.append({"name": "windows:route", "id": 12})
        self.runner["contacted_at"] = "1970-01-01T00:00:01+00:00"
        self.tick()
        self.assertFalse(self.admitted)
        self.runner["contacted_at"] = "1970-01-01T00:17:10+00:00"
        self.tick()
        self.tick()
        self.assertEqual(self.admitted, ["12"])
