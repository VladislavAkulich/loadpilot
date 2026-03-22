// Max throughput: ramp arrival rate from 100 → 10 000 RPS over 10s, hold 20s.
// k6 will hit its VU/CPU ceiling naturally; the plateau RPS in the report is
// the tool's actual throughput limit.
import http from "k6/http";

export const options = {
  scenarios: {
    max: {
      // Ramp arrival rate up to 10k RPS — k6 will hit its ceiling naturally.
      executor: "ramping-arrival-rate",
      startRate: 100,
      timeUnit: "1s",
      stages: [
        { target: 10000, duration: "10s" },
        { target: 10000, duration: "20s" },
      ],
      preAllocatedVUs: 200,
      maxVUs: 1000,
    },
  },
  summaryTrendStats: ["p(50)", "p(95)", "p(99)", "max"],
};

export default function () {
  http.get("http://target:8000/health");
}
