from loadpilot import VUser, scenario, task, LoadClient


# Max throughput: constant 3500 RPS for 30s (well above the expected ceiling).
# The engine fires as many requests as it can; the actual RPS in the report
# reflects the tool's true throughput ceiling on this machine.
@scenario(rps=3_500, duration="30s", mode="constant")
class MaxBench(VUser):
    @task()
    def health(self, client: LoadClient):
        client.get("/health")
