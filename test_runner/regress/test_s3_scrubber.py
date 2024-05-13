import os
import shutil
from typing import Optional

import pytest
from fixtures.common_types import TenantShardId
from fixtures.neon_fixtures import (
    NeonEnvBuilder,
    S3Scrubber,
)
from fixtures.remote_storage import S3Storage, s3_storage
from fixtures.workload import Workload


@pytest.mark.parametrize("shard_count", [None, 4])
def test_scrubber_tenant_snapshot(neon_env_builder: NeonEnvBuilder, shard_count: Optional[int]):
    """
    Test the `tenant-snapshot` subcommand, which grabs data from remote storage

    This is only a support/debug tool, but worth testing to ensure the tool does not regress.
    """

    neon_env_builder.enable_pageserver_remote_storage(s3_storage())
    neon_env_builder.num_pageservers = shard_count if shard_count is not None else 1

    env = neon_env_builder.init_start()
    tenant_id = env.initial_tenant
    timeline_id = env.initial_timeline
    branch = "main"

    # Do some work
    workload = Workload(env, tenant_id, timeline_id, branch)
    workload.init()

    # Multiple write/flush passes to generate multiple layers
    for _n in range(0, 3):
        workload.write_rows(128)

    # Do some more work after a restart, so that we have multiple generations
    for pageserver in env.pageservers:
        pageserver.stop()
        pageserver.start()

    for _n in range(0, 3):
        workload.write_rows(128)

    # If we're doing multiple shards, split: this is important to exercise
    # the scrubber's ability to understand the references from child shards to parent shard's layers
    if shard_count is not None:
        tenant_shard_ids = env.storage_controller.tenant_shard_split(
            tenant_id, shard_count=shard_count
        )

        # Write after shard split: this will result in shards containing a mixture of owned
        # and parent layers in their index.
        workload.write_rows(128)
    else:
        tenant_shard_ids = [TenantShardId(tenant_id, 0, 0)]

    output_path = neon_env_builder.test_output_dir / "snapshot"
    os.makedirs(output_path)

    scrubber = S3Scrubber(neon_env_builder)
    scrubber.tenant_snapshot(tenant_id, output_path)

    assert len(os.listdir(output_path)) > 0

    workload.stop()

    # Stop pageservers
    for pageserver in env.pageservers:
        pageserver.stop()

    # Drop all shards' local storage
    for tenant_shard_id in tenant_shard_ids:
        pageserver = env.get_tenant_pageserver(tenant_shard_id)
        shutil.rmtree(pageserver.timeline_dir(tenant_shard_id, timeline_id))

    # Replace remote storage contents with the snapshot we downloaded
    assert isinstance(env.pageserver_remote_storage, S3Storage)

    remote_tenant_path = env.pageserver_remote_storage.tenant_path(tenant_id)

    # Delete current remote storage contents
    bucket = env.pageserver_remote_storage.bucket_name
    remote_client = env.pageserver_remote_storage.client
    deleted = 0
    for object in remote_client.list_objects_v2(Bucket=bucket, Prefix=remote_tenant_path)[
        "Contents"
    ]:
        key = object["Key"]
        remote_client.delete_object(Key=key, Bucket=bucket)
        deleted += 1
    assert deleted > 0

    # Upload from snapshot
    for root, _dirs, files in os.walk(output_path):
        for file in files:
            full_local_path = os.path.join(root, file)
            full_remote_path = (
                env.pageserver_remote_storage.tenants_path()
                + "/"
                + full_local_path.removeprefix(f"{output_path}/")
            )
            remote_client.upload_file(full_local_path, bucket, full_remote_path)

    for pageserver in env.pageservers:
        pageserver.start()

    # Check we can read everything
    workload.validate()
