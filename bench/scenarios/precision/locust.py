from locust import HttpUser, task, constant_throughput


class PrecisionBench(HttpUser):
    # 500 users × 1 req/s = 500 RPS target
    wait_time = constant_throughput(1)

    @task
    def health(self):
        self.client.get("/health")
