import { defineRailway, postgres, project, service } from "railway/iac";

// Choose placement before provisioning the first database volume.
const region = "us-west2";
export const partial = "workspace";

export default defineRailway((ctx) => {
  const db = postgres("postgres", { region });
  const web = service("web", {
    build: { builder: "DOCKERFILE", dockerfilePath: "Dockerfile" },
    deploy: {
      startCommand: "env -u MIGRATION_DATABASE_URL solo-tracker serve",
      preDeployCommand: [
        `sh -c 'export DATABASE_URL="$MIGRATION_DATABASE_URL" APP_GRANT_RUNTIME=1 && exec solo-tracker migrate'`,
      ],
      preDeployTimeoutSeconds: 300,
      healthcheckPath: "/readyz",
      healthcheckTimeout: 120,
    },
    replicas: { [region]: 1 },
    env: {
      APP_ENV: "production",
      APP_BASE_URL: ctx.shared.APP_BASE_URL,
      DATABASE_URL: ctx.shared.WORKSPACE_RUNTIME_DATABASE_URL,
      MIGRATION_DATABASE_URL: db.env.DATABASE_URL,
      MAIL_FROM: ctx.shared.MAIL_FROM,
      RESEND_API_KEY: ctx.shared.RESEND_API_KEY,
    },
  });
  return project("solo-tracker", { resources: [db, web] });
});
