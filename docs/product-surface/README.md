# The product surface

Every screen of the acceptance journey, captured by
`crates/compute-cli/tests/product_journey.rs`: on a clean machine, `compute`
alone launches the control plane, and everything after that happens in its
UI. Regenerate them with:

```sh
COMPUTE_JOURNEY_SCREENSHOTS=$PWD/docs/product-surface \
  cargo test -p compute-cli --test product_journey
```

See [environment-control-plane.md](../environment-control-plane.md) for the
model behind them.

## 1. Home

`compute` opens here: what do you want to do? The software and the computers it runs on are the first things on screen.

![Home](01-home.png)

## 2. Run a project

A Git repository or a local folder, and the computer to run it on: one you have, or a new one.

![Run a project](02-run-a-project.png)

## 3. The proposed assembly

Compute inspected the source inside the computer and proposes how to run it: build, tests, start command, port. What will happen is spelled out; nothing changes until GO.

![The proposed assembly](03-proposed-assembly.png)

## 4. After GO

Work mode shows what Compute is doing: checkout, build, start, item by item.

![After GO](04-work-reconciling.png)

## 5. Running

The application is built and running, with its endpoint, in the computer.

![Running](05-work-application-running.png)

## 6. Another project, the same computer

A second project added the same way runs beside the first on one machine.

![Another project, the same computer](06-two-projects-one-computer.png)

## 7. Create a computer

What kind of computer do you need — not which provider. Placement chooses the target.

![Create a computer](07-create-a-computer.png)

## 8. Local changes

A new commit pulled and a configuration value set, held locally and listed before GO.

![Local changes](08-local-changes-before-go.png)

## 9. Build and test

The project's build and tests run in its computer; output and job evidence on screen.

![Build and test](09-build-and-test.png)

## 10. Publish a new version

What publishing will do: commit, build, tests, checks, package, version.

![Publish a new version](10-publish-review.png)

## 11. Published

Every step with its evidence; the version is immutable.

![Published](11-version-published.png)

## 12. A version

Source, commit, package digest, who published it and from where, and the evidence of each step.

![A version](12-version-evidence.png)

## 13. Deploy

The version and the environment, and what will change there — in place, no new machine.

![Deploy](13-deploy-review.png)

## 14. Deployed to test

Checkout, build, restart, health check: the rollout's steps as they happened.

![Deployed to test](14-deployed-to-test.png)

## 15. Test's reality

Manage mode: the computer, the software at its version, applications with health and endpoints, and the deployments.

![Test's reality](15-test-reality.png)

## 16. Promote to production

What runs in test and in production, what will change, configuration differences, health, authority, and approvals, before GO.

![Promote to production](16-promote-review.png)

## 17. Promoted

The exact version, made real in production, step by step.

![Promoted](17-promoted-to-production.png)

## 18. Operate production

Health, endpoints, restart, logs, versions, deployments, and the computer's lifecycle.

![Operate production](18-operate-production.png)

## 19. Versions and history

Where the project runs, at which version; every version and every deployment.

![Versions and history](19-software-versions-and-history.png)

## 20. Roll back

Choose a version, review, GO.

![Roll back](20-rollback-review.png)

## 21. Rolled back

The previous version runs again, on the same machine.

![Rolled back](21-rolled-back.png)

## 22. Stop and resume

The computer stopped; Start resumes the same machine.

![Stop and resume](22-computer-stopped.png)

## 23. Terminal

Commands run inside the computer as durable jobs.

![Terminal](23-terminal.png)

## 24. Files

The computer's files.

![Files](24-files.png)

## 25. An agent

An agent runs beside the software, on the same computer.

![An agent](25-agent-running.png)

## 26. Environments

Manage mode: every environment.

![Environments](26-manage-environments.png)

## 27. Try software

A temporary computer for trying something: it expires, and its evidence remains.

![Try software](27-temporary-computer.png)

## 28. Home, afterwards

The software, where it runs and at which version, and the computers.

![Home, afterwards](28-home-with-software.png)
