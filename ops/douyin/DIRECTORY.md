# 抖音服务（lyrabot-douyin）

源码与服务器 `/opt/lyrabot-douyin` 同步；`lyrabot-upload.ps1` 为本机成片入库脚本。


数据根目录为 `/var/lib/lyrabot-douyin`，有效数据不是临时文件。

- `releases/<期号>/`：原成片和清单，同编号不覆盖不同内容。
- `published-records/<期号>/manifest.json`：已发布作品的原清单，只用于身份核对，不是可上传作品包（见下文“发布后的保留策略”）。
- `covers/<期号>/uploads/<封面ID>/`：本人上传图片。
- `covers/<期号>/frames/<封面ID>/`：视频提帧。
- `covers/<期号>/reused/<封面ID>/`：从持久库为目标作品重新绑定的封面。
- `covers/<期号>/saved/<封面ID>/`：从固定制作目录导入的单图。
- `covers/<期号>/pairs/<封面ID>/`：已制作的横 4:3、竖 3:4 两源配对，schema2 只含两张对应 JPEG 与 `cover.json`，清单引用有效单图；不得删掉引用来源。
- 单图 schema1 包含 `source.jpg`、`landscape.jpg`（4:3）、`portrait.jpg`（3:4）及 `cover.json`。原图是已规范化副本；单图转换可能补边，不保证两个槽都满版。不要直接编辑或覆盖这些文件。
- `content-jobs/`、`content-plans/`：任务、计划、引用与防重数据。结果未知的提交不能删除、清零或重投。
- `chromium-profile/`：账号登录与浏览器持久资料，不能当作缓存整体清理。
- `.cache/`、`.config/`、`.local/`：浏览器及虚拟显示运行资料；仅清理经过核实的可再生缓存。

## 发布后的保留策略

只为 `releases/` 中待发布的期号制作封面；期号不在待发布列表里（例如已经发布过）时先向本人确认，不要先做。

某期发布回执为 `published` 后：

1. 用任务与作品编号确认 `content-jobs/` 中有该期 `published` 回执，再核对清单中的 `video_sha256` 与原片计算值一致后，把 `releases/<期号>/manifest.json` 移入 `published-records/<期号>/`（`douyin-runner:lyrabot-douyin`，0600），删除原片和空的作品目录。
2. 删除这一期的全部封面：`covers/<期号>/` 与 `/var/lib/lyrabot/workspace/starlibrary_covers/<期号>/`。删除前确认没有其他待发布作品的配对引用了这一期的单图（`cover_pair` 可跨期引用，源图缺失会让对方的封面失效）；有引用时先保留被引用的单图。`/var/lib/lyrabot/media/douyin/` 中的预览小图不手动删，由媒体清理在 24 小时后回收（仍待投递的会被保留）。
3. 保留 `content-jobs/`、`content-plans/` 中的任务与计划记录（防重依据，含作废计划的记录），以及 `starlibrary_covers/` 根目录的 `templates/`、`cover_template.py` 与 `README.md`；下一期用模板重新制作封面。
4. 结果未知的提交（`needs_attention` 且 `commit_attempted`）在人工核实前不清理。

按此清理后，封面库（`cover_library`）里只剩待发布作品的封面。`cover_reuse` 可把库中仍在的封面重新绑定到目标作品（目标 `release_id`、来源 `from_release_id`），跨期不能直接套用源封面 ID，复用不触发上传或发布。

素材根目录 `/var/lib/lyrabot/workspace/starlibrary_covers/` 的 `templates/` 保存无期号副标题的
双比例母版；根目录内的 `cover_template.py` 使用 Maple Mono NF CN 排入期号和副标题，并将
多比例素材按需生成到 `<期号>/`。
`cover_saved_sources` 只枚举规范的 `<期号>_3x4`、`_4x3`、`_9x16`、`_16x9` 图片，
不移动、删除原件，不读取 zip/master。使用真实 4:3 与 3:4 原图各导入一次，再用
`cover_pair` 配对，检查双槽预览后选择该 ID；`cover_import_saved` 导入的单图只校验、不发预览，配对后由 `cover_pair` 发出两张双槽预览；图库浏览、复用、QQ 图片与视频提帧仍各自附预览。9:16 不替代竖封面 3:4。

旧 `covers/<期号>/<封面ID>/` 仍兼容。分类迁移不改图片、ID 和清单；不能同时保留同 ID 的多份目录。
发布确认由用户发送“确认发布 <plan_id>”后程序直接提交，参数与校验见 LyraBot `docs/06-tools-plugins.md` 第 3 节。
