from loadpilot import VUser, scenario, task, LoadClient


# ── Precision (500 RPS target) ─────────────────────────────────────────────────

@scenario(rps=500, duration="30s", mode="constant")
class PyO3OnStart(VUser):
    """on_start only — per-VUser login, async task."""

    def on_start(self, client: LoadClient):
        resp = client.post("/auth/login", json={})
        self.token = resp.json()["access_token"]

    @task()
    async def get_user(self, client: LoadClient):
        client.get("/api/user", headers={"Authorization": f"Bearer {self.token}"})


@scenario(rps=500, duration="30s", mode="constant")
class PyO3Full(VUser):
    """on_start + check_* — full PyO3 round-trip."""

    def on_start(self, client: LoadClient):
        resp = client.post("/auth/login", json={})
        self.token = resp.json()["access_token"]

    @task()
    async def get_user(self, client: LoadClient):
        client.get("/api/user", headers={"Authorization": f"Bearer {self.token}"})

    def check_get_user(self, response) -> None:
        assert response.status_code == 200
        assert response.json()["id"] == 1


# ── Max throughput ─────────────────────────────────────────────────────────────

@scenario(rps=3_500, duration="30s", mode="constant", ramp_up="0s")
class PyO3MaxOnStart(VUser):
    """Max throughput with on_start + async task."""

    def on_start(self, client: LoadClient):
        resp = client.post("/auth/login", json={})
        self.token = resp.json()["access_token"]

    @task()
    async def get_user(self, client: LoadClient):
        client.get("/api/user", headers={"Authorization": f"Bearer {self.token}"})


@scenario(rps=3_500, duration="30s", mode="constant", ramp_up="0s")
class PyO3MaxFull(VUser):
    """Max throughput with on_start + async task + check_*."""

    def on_start(self, client: LoadClient):
        resp = client.post("/auth/login", json={})
        self.token = resp.json()["access_token"]

    @task()
    async def get_user(self, client: LoadClient):
        client.get("/api/user", headers={"Authorization": f"Bearer {self.token}"})

    def check_get_user(self, response) -> None:
        assert response.status_code == 200
        assert response.json()["id"] == 1
