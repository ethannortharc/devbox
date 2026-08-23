# devbox v4 examples

These files are executable examples, not illustrative fragments. From a clean checkout:

```bash
# Parse, allocate, and inspect without a runtime.
devbox lab status examples/labs/mini-clos.toml
devbox lab config examples/labs/mini-clos.toml

# Preview every privileged operation.
devbox lab up examples/labs/mini-clos.toml --substrate LAB_BOX --dry-run

# Run it in an existing Linux-capable devbox, then inject and heal a partition.
devbox lab up examples/labs/mini-clos.toml --substrate LAB_BOX
devbox lab fault examples/labs/mini-clos.toml leaf1-spine1 --partition --substrate LAB_BOX
devbox lab heal examples/labs/mini-clos.toml leaf1-spine1 --substrate LAB_BOX
devbox lab down examples/labs/mini-clos.toml --substrate LAB_BOX
```

`labs/ztp-fabric.toml` starts two blank leaves, obtains DHCP options 66/67, downloads generated FRR configuration from `devbox-ztpd`, and waits for provisioning plus routed reachability before `lab up` succeeds.

The policy files are complete minimal `devbox.toml` configurations. Copy one to a scratch project or merge its `[policy]` section into an existing project, then run `devbox policy test <host-or-IP>` before applying it to a running box.

CI parses every example, derives all address plans and router configurations, and renders every privileged command. That keeps these files runnable as the schema evolves.
