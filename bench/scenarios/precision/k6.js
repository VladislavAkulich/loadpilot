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
