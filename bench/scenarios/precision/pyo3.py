from loadpilot import VUser, scenario, task, LoadClient


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
