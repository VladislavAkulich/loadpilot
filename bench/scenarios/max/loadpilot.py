from loadpilot import VUser, scenario, task, LoadClient


# Step profile: 5 steps from 0 to 3000 RPS over 30s.
# Finds the natural ceiling — errors appear when throughput is saturated.
@scenario(rps=3_500, duration="30s", mode="constant")
class MaxBench(VUser):
    @task()
    def health(self, client: LoadClient):
        client.get("/health")
