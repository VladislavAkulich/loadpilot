// Precision: constant-arrival-rate at 500 RPS for 30s.
// k6 pre-allocates VUs and keeps the arrival rate fixed regardless of latency.
// Measures scheduler accuracy and generator-side latency overhead.
import http from "k6/http";

export const options = {
  scenarios: {
    precision: {
      executor: "constant-arrival-rate",
      rate: 500,
      timeUnit: "1s",
      duration: "30s",
      preAllocatedVUs: 50,
      maxVUs: 200,
    },
  },
  summaryTrendStats: ["p(50)", "p(95)", "p(99)", "max"],
};

export default function () {
  http.get("http://target:8000/health");
}
