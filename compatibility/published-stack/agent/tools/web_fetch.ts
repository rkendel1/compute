// Removed from the default configured agent. Chip's built-in shell, file and web tools carry no scope or
// delegation enforcement, and their reach must not depend on whether an optional sandbox backend happens to
// be installed. An operator who wants one re-adds it deliberately, with its own authority.
import { disableTool } from "@appport/chip/tools";

export default disableTool();
