"""
Basic LoadPilot scenario — simulates a checkout flow against a demo shop API.

Run it:
    loadpilot run examples/basic_scenario.py --target http://localhost:8000

Or dry-run to inspect the generated plan JSON:
    loadpilot run examples/basic_scenario.py --target http://localhost:8000 --dry-run
"""

import random

from loadpilot import LoadClient, scenario, task


@scenario(rps=50, duration="2m", ramp_up="30s")
class CheckoutFlow:
    """Simulates a realistic checkout flow: browse → view → add to cart."""

    def on_start(self, client: LoadClient):
        """Called once per virtual user before tasks begin."""
        resp = client.post(
            "/auth/login",
            json={
                "username": f"user_{random.randint(1, 1000)}",
                "password": "testpass",
            },
        )
        self.token = resp.json().get("token", "demo-token")

    @task(weight=5)
    def browse_products(self, client: LoadClient):
        """Most frequent task — browse the product listing."""
        client.get(
            "/api/products",
            headers={"Authorization": f"Bearer {self.token}"},
        )

    @task(weight=2)
    def view_product(self, client: LoadClient):
        """View a random product detail page."""
        product_id = random.randint(1, 100)
        client.get(
            f"/api/products/{product_id}",
            headers={"Authorization": f"Bearer {self.token}"},
        )

    @task(weight=1)
    def add_to_cart(self, client: LoadClient):
        """Least frequent task — add an item to cart."""
        client.post(
            "/api/cart",
            json={"product_id": random.randint(1, 100), "qty": 1},
            headers={"Authorization": f"Bearer {self.token}"},
        )


@scenario(rps=20, duration="1m", ramp_up="10s")
class HealthCheckFlow:
    """Lightweight scenario for monitoring — just hits the health endpoint."""

    @task(weight=1)
    def health(self, client: LoadClient):
        client.get("/health")
