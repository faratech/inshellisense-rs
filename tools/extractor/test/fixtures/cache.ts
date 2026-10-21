const completionSpec = {
  name: "cache-fixture",
  args: {
    generators: [
      {
        script: "printf branches",
        cache: { ttl: 1500, strategy: "max-age", cacheByDirectory: true },
      },
      {
        script: ["printf", "packages"],
        cache: { ttl: 30000, strategy: "stale-while-revalidate", cacheByDirectory: false },
      },
      { script: "printf uncached" },
      {
        script: ["printf", "legacy"],
        cache: { ttlSeconds: 2, strategy: "max_age", byDir: true },
      },
      { script: "printf zero-ttl", cache: { ttl: 0 } },
    ],
  },
};

export default completionSpec;
