from loadpilot import VUser, scenario, task, LoadClient

# Precision: constant 500 RPS for 30s, pure Rust (no Python callbacks).
# Measures how accurately LoadPilot holds the target rate and what latency
# overhead the load generator itself adds — no on_start, no check_*.
@scenario(rps=500, duration="30s", mode="constant")
class PrecisionBench(VUser):
    @task()
    def health(self, client: LoadClient):
        client.get("/health")
