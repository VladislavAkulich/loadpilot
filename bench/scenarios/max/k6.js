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
