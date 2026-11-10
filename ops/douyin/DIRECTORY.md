# 抖音服务（lyrabot-douyin）

源码与服务器 `/opt/lyrabot-douyin` 同步；`lyrabot-upload.ps1` 为本机成片入库脚本。


数据根目录为 `/var/lib/lyrabot-douyin`，有效数据不是临时文件。

- `releases/<期号>/`：原成片和清单，同编号不覆盖不同内容。
- `published-records/<期号>/manifest.json`：明确要求删除服务器原片后保留的原清单；只用于图库身份核对，不是可上传作品包。应在发布回执与列表核对后移入，删除原视频和空作品目录；任务防重记录必须保留，封面默认保留，除非本人另行要求清理。
- `covers/<期号>/uploads/<封面ID>/`：本人上传图片。
- `covers/<期号>/frames/<封面ID>/`：视频提帧。
- `covers/<期号>/reused/<封面ID>/`：从持久库为目标作品重新绑定的封面。
- `covers/<期号>/saved/<封面ID>/`：从固定制作目录导入的单图。
- `covers/<期号>/pairs/<封面ID>/`：已制作的横 4:3、竖 3:4 两源配对，schema2 只含两张对应 JPEG 与 `cover.json`，清单引用有效单图；不得删掉引用来源。
- 单图 schema1 包含 `source.jpg`、`landscape.jpg`（4:3）、`portrait.jpg`（3:4）及 `cover.json`。原图是已规范化副本；单图转换可能补边，不保证两个槽都满版。不要直接编辑或覆盖这些文件。
- `content-jobs/`、`content-plans/`：任务、计划、引用与防重数据。结果未知的提交不能删除、清零或重投。
- `chromium-profile/`：账号登录与浏览器持久资料，不能当作缓存整体清理。
- `.cache/`、`.config/`、`.local/`：浏览器及虚拟显示运行资料；仅清理经过核实的可再生缓存。

在 QQ 私聊说“查看封面库”，主对话角色交给创作专员用 `cover_library` 分页展示。
本人选择后，`cover_reuse` 校验原图并为目标期号生成对应 ID，再预览、确认和准备。
跨期不能直接套用源封面 ID；封面复用不触发上传或发布。

素材根目录 `/var/lib/lyrabot/workspace/starlibrary_covers/` 的 `templates/` 保存无期号副标题的
双比例母版；根目录内的 `cover_template.py` 使用 Maple Mono NF CN 排入期号和副标题，并将
多比例素材按需生成到 `<期号>/`。三期旧封面已按本人要求清理，不能把历史图库记录当作当前可用素材。
`cover_saved_sources` 只枚举规范的 `<期号>_3x4`、`_4x3`、`_9x16`、`_16x9` 图片，
不移动、删除原件，不读取 zip/master。使用真实 4:3 与 3:4 原图各导入一次，再用
`cover_pair` 配对，检查双槽预览后选择该 ID；9:16 不替代竖封面 3:4。

旧 `covers/<期号>/<封面ID>/` 仍兼容。分类迁移不改图片、ID 和清单；不能同时保留同 ID 的多份目录。
发布确认由用户发送“确认发布 <plan_id>”后程序直接提交，参数与校验见 LyraBot `docs/06-tools-plugins.md` 第 3 节。
