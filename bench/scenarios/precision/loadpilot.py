from loadpilot import VUser, scenario, task, LoadClient


@scenario(rps=500, duration="30s", mode="constant")
class PrecisionBench(VUser):
    @task()
    def health(self, client: LoadClient):
        client.get("/health")
