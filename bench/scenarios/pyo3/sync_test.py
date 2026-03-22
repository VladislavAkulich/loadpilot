from loadpilot import VUser, scenario, task, LoadClient


@scenario(rps=3_500, duration="30s", mode="constant", ramp_up="0s")
class PyO3MaxSync(VUser):
    """Max throughput: on_start + sync task (no asyncio overhead)."""

    def on_start(self, client: LoadClient):
        resp = client.post("/auth/login", json={})
        self.token = resp.json()["access_token"]

    @task()
    def get_user(self, client: LoadClient):
        client.get("/api/user", headers={"Authorization": f"Bearer {self.token}"})
