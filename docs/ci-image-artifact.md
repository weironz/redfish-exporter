# 固定来源 CI 镜像产物

CI 对每个提交构建镜像，OCI revision 和运行时 `--version` 必须同时匹配该次
GitHub Actions 的完整提交 SHA。运行时检查使用 `--network none`、只读文件系统、
删除全部 capabilities 和禁止提权；不挂载凭据或 BMC 配置，不访问设备。

只有 main 的 push 保存 Docker archive、镜像 inspect 元数据和 SHA256SUMS，保留
7 天。PR 和其他分支只验证，不保存可发布产物。整个工作流仅有 contents:read，
不持有镜像仓库或生产凭据，也不自动发布、部署。

人工发布前必须验证：

1. main 的完整提交 SHA 与 CI run 的 headSha 一致，工作流所有质量门禁成功，
   包括 `image-ready`；单独 image job 成功不足以放行。
2. 下载名称包含该完整 SHA 的产物，核对两个文件的 SHA256SUMS。
3. 核对 inspect 的 source/revision、镜像 ID、Linux/amd64 以及归档中的 image
   config；只能向尚未发布的新版本 tag 推送，不覆盖旧 tag。
4. 发布后记录 registry digest，核对远端 config 与 CI 镜像一致。目标端仅从
   镜像仓库拉取，镜像拉取后再次执行隔离 `--version` 校验。
5. CI 成功、镜像发布都不等于生产部署授权。ReadyIDC 灰度与中心 Deploy 仍受
   各自批准及实机门禁约束；不能因为本机 Docker 不可用而在生产构建源码。

本机 Docker Desktop 启动异常时，可以从可信 CI 下载产物，用支持 Docker archive
的仓库客户端推送，不依赖本机 Docker Engine。不得通过清卷或工厂重置绕过故障。
