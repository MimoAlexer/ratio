import assert from "node:assert/strict";
import { createRailwayContext, project } from "railway/iac";
import configure from "./railway.ts";

// Exercise the SDK without an account or a remote plan/apply operation.
for (const environment of ["staging", "production"]) {
  const definition = await configure(createRailwayContext({ environment }), project);
  const database = definition.resources.find((node) => node.type === "database");
  const web = definition.resources.find((node) => node.address === "service.web");
  assert.equal(database.engine, "postgres");
  assert.equal(web.variables.MIGRATION_DATABASE_URL.resource, database.address);
  assert.equal(web.variables.MIGRATION_DATABASE_URL.output, "DATABASE_URL");
  assert.equal(web.variables.DATABASE_URL.name, "WORKSPACE_RUNTIME_DATABASE_URL");
  assert.equal(web.variables.APP_BASE_URL.name, "APP_BASE_URL");
  assert.equal(web.variables.APP_ENV.value, "production");
  assert.equal(web.build.builder, "DOCKERFILE");
  assert.match(web.deploy.preDeployCommand[0], /APP_GRANT_RUNTIME=1 .*solo-tracker migrate/);
  assert.equal(web.deploy.startCommand, "env -u MIGRATION_DATABASE_URL solo-tracker serve");
  assert.equal(web.deploy.healthcheckPath, "/readyz");
  assert.deepEqual(web.deploy.multiRegionConfig, database.deploy.multiRegionConfig);
  assert.equal(Object.values(web.deploy.multiRegionConfig)[0].numReplicas, 1);
  console.log(`${environment}: PostgreSQL, private database reference, migration and web readiness verified`);
}
