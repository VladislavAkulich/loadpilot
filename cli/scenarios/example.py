"""
Example LoadPilot scenario.

Run:
    loadpilot run scenarios/example.py --target http://localhost:8000
"""

from loadpilot import LoadClient, VUser, scenario, task


@scenario(
    rps=20,
    duration="1m",
    ramp_up="10s",
    thresholds={"p99_ms": 500, "error_rate": 1.0},
)
class ExampleFlow(VUser):
    """Hits /health (weight 3) and / (weight 1)."""

    @task(weight=3)
    def health(self, client: LoadClient):
        client.get("/health")

    @task(weight=1)
    def root(self, client: LoadClient):
        client.get("/")
