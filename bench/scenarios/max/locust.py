from locust import HttpUser, task


class MaxBench(HttpUser):
    # No wait_time — each user fires requests back-to-back as fast as possible.
    # 200 users × (1000ms / latency_ms) ≈ max achievable RPS for Locust.

    @task
    def health(self):
        self.client.get("/health")
