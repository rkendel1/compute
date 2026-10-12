import { defineMcpClientConnection } from "@appport/chip/connections";

export default defineMcpClientConnection({
  url: process.env.APPBOUNDRY_MCP_URL ?? "http://127.0.0.1:4400/apps/todos/mcp",
  description: "AppBoundry todos application: create, list, complete and delete durable todo items.",
  auth: { getToken: async () => ({ token: process.env.APPPORT_MCP_TOKEN! }) },
});
