from loadpilot import VUser, scenario, task, LoadClient


@scenario(rps=3_500, duration="30s", mode="constant", ramp_up="0s")
class PyO3MaxOnStart(VUser):
    """Max throughput: on_start + async task."""

    def on_start(self, client: LoadClient):
        resp = client.post("/auth/login", json={})
        self.token = resp.json()["access_token"]

    @task()
    async def get_user(self, client: LoadClient):
        client.get("/api/user", headers={"Authorization": f"Bearer {self.token}"})


@scenario(rps=3_500, duration="30s", mode="constant", ramp_up="0s")
class PyO3MaxFull(VUser):
    """Max throughput: on_start + async task + check_*."""

    def on_start(self, client: LoadClient):
        resp = client.post("/auth/login", json={})
        self.token = resp.json()["access_token"]

    @task()
    async def get_user(self, client: LoadClient):
        client.get("/api/user", headers={"Authorization": f"Bearer {self.token}"})

    def check_get_user(self, status_code: int, body) -> None:
        assert status_code == 200
        assert body["id"] == 1


@scenario(rps=3_500, duration="30s", mode="constant", ramp_up="0s")
class PyO3MaxSync(VUser):
    """Max throughput: on_start + sync task (no asyncio overhead)."""

    def on_start(self, client: LoadClient):
        resp = client.post("/auth/login", json={})
        self.token = resp.json()["access_token"]

    @task()
    def get_user(self, client: LoadClient):
        client.get("/api/user", headers={"Authorization": f"Bearer {self.token}"})


@scenario(rps=700, duration="30s", mode="constant", ramp_up="0s")
class PyO3Batch5(VUser):
    """Pure-Rust batch: one PyO3 call → 5 concurrent HTTP via tokio JoinSet.

    700 task-dispatches × 5 HTTP = 3 500 effective HTTP/sec.
    """

    def on_start(self, client: LoadClient):
        resp = client.post("/auth/login", json={})
        self.token = resp.json()["access_token"]

    @task()
    def fetch_batch(self, client: LoadClient):
        auth = {"Authorization": f"Bearer {self.token}"}
        client.batch([
            {"method": "GET", "path": "/api/user", "headers": auth},
            {"method": "GET", "path": "/api/user", "headers": auth},
            {"method": "GET", "path": "/api/user", "headers": auth},
            {"method": "GET", "path": "/api/user", "headers": auth},
            {"method": "GET", "path": "/api/user", "headers": auth},
        ])
