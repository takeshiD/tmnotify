# tmnotify

tmux 内で発生した出来事を、利用者が現在見ている場所へ通知し、必要に応じて発生元へ戻れるようにする通知センター。

## Language

**Notification**:
利用者に知らせる一件の出来事。表示の有無や表示先とは独立して履歴に残り得る。
_Avoid_: Message, alert

**Toast**:
操作対象を奪わず、一定時間だけ表示される、表示面からは操作できない Notification。Source Pane への移動は History から行う。
_Avoid_: Popup

**Attention Gate**:
利用者の操作を一時的に止め、Notification の Source Pane へ移動する機会を与える、時間切れのない表示。外部ツールに対する意思決定そのものは扱わない。
_Avoid_: Decision dialog, approval UI

**Notification Key**:
同じ進行中の出来事について送られた複数の Notification を対応付ける、producer が指定する論理的な識別子。後から届いた内容で既存の Notification を更新するために使う。
_Avoid_: Deduplication key, database ID

Notification Key が付いた live Notification は、`tmnotify jump --key` から
Source Pane へ直接戻る対象にもできる。これはキーボードの key binding とは
異なる概念であり、Notification Key を tmux command や shell command に展開しない。

**Source Pane**:
Notification の原因となった処理が実行されていた tmux pane。Notification の表示場所とは限らない。
_Avoid_: Target pane

**Attention Window**:
Notification を利用者に見せる tmux window。Source Pane の所属 window とは独立して選ばれ、一つの Notification が複数の Attention Window に表示され得る。
_Avoid_: Source window

**Window Display**:
一つの Notification を一つの Attention Window に表示したもの。同じ window を見ている複数の tmux client は同じ Window Display を共有する。
_Avoid_: Display replica, notification copy

**Hook Preset**:
agent provider のどの lifecycle event を Notification に変換するかをまとめた設定。既定は attention、完了、失敗だけを含み、個別の event を追加または除外できる。
_Avoid_: Hook installation

**History**:
永続化された過去の Notification の集合。既定では、現在接続している tmux server に属する Notification だけを指す。
_Avoid_: Live queue

**Dismiss**:
表示中の Notification を閉じる操作。History からは削除しない。
_Avoid_: Delete

**Hide**:
Notification を通常の History 一覧から除外する、取り消し可能な操作。
_Avoid_: Dismiss, delete

**Clear**:
対象となる Notification を History から物理的に削除する操作。
_Avoid_: Dismiss, hide

**Hook Installation**:
特定の agent provider と scope に置かれた、tmnotify が所有する lifecycle hook handler。Hook Preset や provider による trust とは独立している。
_Avoid_: Hook preset, hook trust
