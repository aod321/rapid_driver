# iPhone ↔ 树莓派 通信协议文档

## 目录

- [1. 系统概览](#1-系统概览)
- [2. 服务发现 (Bonjour/mDNS)](#2-服务发现-bonjourmdns)
- [3. TCP 数据流协议](#3-tcp-数据流协议)
- [4. 树莓派端数据管线](#4-树莓派端数据管线)
- [5. MCAP 录制格式](#5-mcap-录制格式)
- [6. HTTP 控制 API](#6-http-控制-api)
- [7. 回放系统](#7-回放系统)
- [8. 线程与调度模型](#8-线程与调度模型)
- [9. 关键源文件索引](#9-关键源文件索引)

---

## 1. 系统概览

```
┌─────────────────────┐          WiFi (TCP)          ┌──────────────────────────────────┐
│      iPhone          │ ◄──── Bonjour 互相发现 ────► │         Raspberry Pi              │
│                      │                              │                                  │
│  ARKit 30fps 采集    │ ── TCP 二进制帧流 ─────────► │  node_iphone 进程                │
│  - 6DOF 位姿         │    (pose + JPEG)             │    ↓ msgpack 编码                │
│  - 相机图像 (JPEG)   │                              │    ↓ ZMQ PUB :5563               │
│                      │                              │    ↓                              │
│  RecordingController │ ── HTTP API ──────────────► │  MCAP Recorder                   │
│  (录制控制)          │    (port 7400)               │    → .mcap 文件 (Zstd 压缩)      │
└─────────────────────┘                              └──────────────────────────────────┘
```

**传输介质：** WiFi 局域网，无固定 IP，通过 mDNS 自动发现。

---

## 2. 服务发现 (Bonjour/mDNS)

iPhone 和树莓派通过三个 Bonjour 服务类型互相发现：

| 服务类型 | 广播方 | 发现方 | 用途 |
|---|---|---|---|
| `_iphonevio._tcp` | iPhone | 树莓派 | iPhone 数据源，通知树莓派有 iPhone 在线 |
| `_vioserver._tcp` | 树莓派 | iPhone | 数据接收端，iPhone 发现后自动建立 TCP 连接 |
| `_rapiddriver._tcp` | 树莓派 | iPhone | HTTP 控制 API，用于录制控制和设备状态查询 |

### 2.1 iPhone 广播 `_iphonevio._tcp`

- **服务名称：** `{deviceModel}-iPhoneVIO`（如 `iPhone16,2-iPhoneVIO`）
- **端口：** 系统分配的临时端口（ephemeral port）
- **TXT Records：**

| Key | Value | 说明 |
|---|---|---|
| `sessionId` | UUID v4 字符串 | 每次重连时重新生成 |
| `deviceModel` | 设备型号标识 | 来自 `uname()` 系统调用 |
| `appVersion` | App 版本号 | 来自 `CFBundleShortVersionString` |

- **源文件：** `iPhoneVIO/BonjourManager.swift:50-68`

### 2.2 iPhone 发现 `_vioserver._tcp`

- **浏览域：** `.local`（本地网络）
- **Peer-to-Peer：** 开启（`includePeerToPeer = true`）
- **发现后行为：** 自动连接第一个发现的服务端

### 2.3 iPhone 发现 `_rapiddriver._tcp`

- **用途：** 解析出 HTTP 控制 API 的 `http://{ip}:{port}` 地址
- **地址解析：** 强制 IPv4（`ipOptions.version = .v4`）
- **默认端口：** 7400

### 2.4 树莓派 mDNS 发现

- **库：** `mdns-sd` crate
- **Settlement 延迟：** 发现后等待 2 秒确认设备稳定在线
- **移除宽限期：** 设备消失后 5 秒宽限期，防止瞬断误判

---

## 3. TCP 数据流协议

iPhone 通过 TCP 将 ARKit 数据实时发送到树莓派。

### 3.1 连接参数

| 参数 | 值 |
|---|---|
| 协议 | TCP（无 TLS） |
| TCP_NODELAY | `true`（禁用 Nagle 算法，降低延迟） |
| QoS | `userInitiated`（前台优先级） |
| 背压控制 | `isSending` 标志位，发送未完成时跳过当前帧 |
| 重连策略 | 断线后 2.0 秒自动重连 |

### 3.2 消息封装格式

所有消息共用 8 字节固定头部：

```
偏移    大小       类型         说明
─────────────────────────────────────────
0       4 bytes   uint32 LE    载荷长度 (payload length)
4       1 byte    uint8        消息类型 (message type)
5       3 bytes   -            保留 (0x00)
8+      N bytes   -            载荷数据
```

**消息类型：**

| 值 | 类型 | 说明 |
|---|---|---|
| `0x00` | SessionMetadata | 会话元数据（每次连接发送一次） |
| `0x01` | FrameData | 帧数据（每帧发送一次） |

### 3.3 SessionMetadata 载荷 (type=0x00)

**格式：** JSON 编码

**发送时机：** TCP 连接建立后、收到第一个 ARKit 帧时发送一次。

```json
{
  "sessionId":       "550e8400-e29b-41d4-a716-446655440000",
  "deviceModel":     "iPhone16,2",
  "imageWidth":      1920,
  "imageHeight":     1440,
  "focalLengthX":    1597.82,
  "focalLengthY":    1597.82,
  "principalPointX": 960.0,
  "principalPointY": 720.0,
  "arkitTimestamp0":  182735.123456789,
  "wallClock0":       1709000000.123
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `sessionId` | string | UUID v4，标识本次录制会话 |
| `deviceModel` | string | iPhone 硬件型号 |
| `imageWidth` | int | 图像宽度（像素） |
| `imageHeight` | int | 图像高度（像素） |
| `focalLengthX` | float | X 方向焦距（像素） |
| `focalLengthY` | float | Y 方向焦距（像素） |
| `principalPointX` | float | 主点 X 坐标（像素） |
| `principalPointY` | float | 主点 Y 坐标（像素） |
| `arkitTimestamp0` | double | ARKit 设备时间戳基准（纳秒） |
| `wallClock0` | double | Unix 墙钟时间基准（秒） |

**相机内参来源：** `ARFrame.camera.intrinsics`（3×3 矩阵）

```
intrinsics = ┌ fx   0   cx ┐
             │  0  fy   cy │
             └  0   0    1 ┘

focalLengthX  = intrinsics[0][0]  (fx)
focalLengthY  = intrinsics[1][1]  (fy)
principalPointX = intrinsics[2][0]  (cx)
principalPointY = intrinsics[2][1]  (cy)
```

### 3.4 FrameData 载荷 (type=0x01)

**格式：** 自定义二进制，小端序

**发送频率：** ~30 FPS（跟随 ARKit 帧率，受背压控制约束）

```
偏移       大小        类型           说明
───────────────────────────────────────────────────────────────
0          4 bytes    uint32 LE      JPEG 图像数据长度
4          64 bytes   float32[16]    4×4 相机位姿矩阵（列主序）
68         8 bytes    float64        ARKit 设备时间戳
76         8 bytes    float64        Unix 墙钟时间戳（秒）
84         N bytes    raw bytes      JPEG 图像数据
───────────────────────────────────────────────────────────────
总计: 84 + N bytes
```

#### 位姿矩阵 (Transform Matrix)

- **类型：** `simd_float4x4`（来自 `ARFrame.camera.transform`）
- **大小：** 16 × 4 bytes = 64 bytes
- **存储顺序：** 列主序（column-major），即 `transform[col][row]`
- **坐标系：** ARKit 世界坐标系（右手系，Y 轴朝上）

```
T = ┌ r00  r01  r02  tx ┐
    │ r10  r11  r12  ty │
    │ r20  r21  r22  tz │
    └  0    0    0    1  ┘

二进制存储: [r00, r10, r20, 0, r01, r11, r21, 0, r02, r12, r22, 0, tx, ty, tz, 1]
             ← col 0 →      ← col 1 →      ← col 2 →      ← col 3 →
```

#### JPEG 图像

| 参数 | 值 |
|---|---|
| 压缩质量 | 0.7（70%，有损压缩） |
| 色彩空间 | Device RGB |
| 编码方法 | `CIContext.jpegRepresentation()` |
| 来源 | `ARFrame.capturedImage`（CVPixelBuffer） |
| 编码线程 | 独立 `jpegQueue`（后台 QoS） |

### 3.5 连接生命周期

```
iPhone                                     树莓派
  │                                          │
  │ ←── Bonjour 发现 _vioserver._tcp ──────  │
  │                                          │
  │ ── TCP connect ──────────────────────►   │
  │                                          │
  │    [等待第一个 ARKit 帧]                  │
  │                                          │
  │ ── SessionMetadata (type=0x00) ──────►   │
  │                                          │
  │ ── FrameData (type=0x01) ────────────►   │  ← 每帧 ~33ms
  │ ── FrameData (type=0x01) ────────────►   │
  │ ── FrameData (type=0x01) ────────────►   │
  │ ── ...                                   │
  │                                          │
  │    [网络断开或用户断开]                    │
  │                                          │
  │    [等待 2.0 秒]                          │
  │                                          │
  │ ── TCP reconnect ───────────────────►    │
  │ ── SessionMetadata (新 sessionId) ───►   │
  │ ── FrameData ... ───────────────────►    │
```

---

## 4. 树莓派端数据管线

```
  TCP (from iPhone)
       │
       ▼
  node_iphone 进程
  (解析二进制帧, msgpack 编码)
       │
       ▼
  ZMQ PUB socket
  tcp://127.0.0.1:5563
       │
       ▼
  ZMQ SUB (RecorderCore)
  (rmpv 解码 msgpack → JSON)
       │
       ▼
  MCAP Writer
  (Zstd 压缩, 写入 .mcap 文件)
```

### 4.1 ZMQ 传输层

| 参数 | 值 |
|---|---|
| 模式 | PUB/SUB |
| 地址 | `tcp://127.0.0.1:5563`（默认） |
| 端口来源 | `on_attach` 命令的 `--data-port` 参数，默认 5563 |
| 消息格式 | msgpack 编码的多帧消息（multipart） |

**端口解析逻辑：** 对于 `_iphonevio._tcp` 类型的设备，数据源不是 iPhone 的 TCP 地址，而是本地 `node_iphone` 进程的 ZMQ PUB 端口。

### 4.2 Msgpack → JSON 转换

ZMQ 接收到的 msgpack 消息被解析为 `SensorMessage`：

```rust
struct SensorMessage {
    source_name: String,    // 设备名称（如 "iphone_device"）
    msg_type: String,       // 消息类型（如 "iphone_pose", "image"）
    timestamp_ns: u64,      // 纳秒时间戳
    json_data: Vec<u8>,     // JSON 序列化的载荷
}
```

**解析规则：**
- 顶层结构必须是 msgpack Map
- 提取 `type` 字段作为消息类型
- 提取 `ts` 字段（秒），乘以 `1e9` 转换为纳秒
- 所有 msgpack Binary 值 → Base64 编码字符串
- 其余类型直接映射为对应 JSON 类型

### 4.3 消息类型路由

| `msg_type` | MCAP Topic | Schema | 说明 |
|---|---|---|---|
| `iphone_pose` | `/{device}/pose` | `foxglove.PoseInFrame` | 6DOF 位姿 |
| `image` | `/{device}/image` | `foxglove.CompressedImage` | JPEG 图像 |
| `motor` | `/{device}/state` | `zumi.MotorState` | 电机状态 |
| `video` | `/{device}/video` | `zumi.VideoPacket` | 视频包 |
| `recording_start/stop` | `/{device}/metadata` | `zumi.GoProMetadata` | 录制事件 |

另外，`/hardware_mask` channel 独立写入，频率 10 Hz（从 500 Hz 引擎循环中 50:1 降采样）。

---

## 5. MCAP 录制格式

### 5.1 文件配置

| 参数 | 值 |
|---|---|
| 文件路径 | `./recordings/{session_id}/{session_id}.mcap` |
| 压缩算法 | Zstd |
| Profile | `"zumi"` |
| Session ID | UUID v4 或自定义字符串（最长 64 字符） |

### 5.2 Schema 定义

#### foxglove.PoseInFrame（位姿）

```json
{
  "type": "object",
  "properties": {
    "timestamp": {
      "type": "object",
      "properties": {
        "sec":  { "type": "integer" },
        "nsec": { "type": "integer" }
      }
    },
    "frame_id": { "type": "string" },
    "pose": {
      "type": "object",
      "properties": {
        "position": {
          "type": "object",
          "properties": {
            "x": { "type": "number" },
            "y": { "type": "number" },
            "z": { "type": "number" }
          }
        },
        "orientation": {
          "type": "object",
          "properties": {
            "x": { "type": "number" },
            "y": { "type": "number" },
            "z": { "type": "number" },
            "w": { "type": "number" }
          }
        }
      }
    }
  }
}
```

#### foxglove.CompressedImage（压缩图像）

```json
{
  "type": "object",
  "properties": {
    "timestamp": {
      "type": "object",
      "properties": {
        "sec":  { "type": "integer" },
        "nsec": { "type": "integer" }
      }
    },
    "frame_id": { "type": "string" },
    "format":   { "type": "string" },
    "data":     { "type": "string", "contentEncoding": "base64" }
  }
}
```

#### zumi.HardwareMask（硬件设备状态）

```json
{
  "type": "object",
  "properties": {
    "type":         { "type": "string" },
    "ts":           { "type": "number" },
    "mask":         { "type": "integer" },
    "sequence":     { "type": "integer" },
    "device_count": { "type": "integer" },
    "devices":      { "type": "object" }
  }
}
```

### 5.3 MCAP 消息头部

```
字段            类型       说明
──────────────────────────────────────
channel_id     uint16     Channel 标识
sequence       uint32     每 channel 递增序号
log_time       uint64     纳秒时间戳
publish_time   uint64     同 log_time
```

---

## 6. HTTP 控制 API

树莓派通过 `_rapiddriver._tcp` 广播 HTTP API，默认端口 7400。

### 6.1 录制控制

| 方法 | 路径 | 请求体 | 说明 |
|---|---|---|---|
| `POST` | `/recording/start` | `{"session_id": "可选UUID", "devices": [...], "output_dir": "..."}` | 开始录制 |
| `POST` | `/recording/stop` | `{}` | 停止录制 |
| `GET` | `/recording/status` | - | 查询录制状态 |

### 6.2 录制文件管理

| 方法 | 路径 | 参数 | 说明 |
|---|---|---|---|
| `GET` | `/recordings` | `?sort=name\|size\|created_at&order=asc\|desc` | 列出所有 MCAP 文件 |
| `DELETE` | `/recordings/{session_id}` | - | 删除单个录制文件 |
| `POST` | `/recordings/delete_batch` | `{"session_ids": [...]}` | 批量删除 |

### 6.3 回放控制

| 方法 | 路径 | 参数 | 说明 |
|---|---|---|---|
| `POST` | `/recordings/{session_id}/replay` | `?speed=1.0` | 开始回放 |
| `POST` | `/replay/stop` | - | 停止回放 |
| `GET` | `/replay/status` | - | 查询回放进度 |

### 6.4 设备管理

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/devices` | 查询所有设备状态 |
| `POST` | `/devices/{name}/restart` | 重启指定设备进程 |
| `GET` | `/ready` | 就绪检查：`{"ready": bool, "online": int, "total": int}` |

### 6.5 iPhone App 轮询

- **轮询间隔：** 2.0 秒
- **轮询端点：** `GET /ready`，`GET /devices`
- **错误格式：** `{"error": "描述信息"}`

---

## 7. 回放系统

MCAP 文件可通过 HTTP API 触发回放，数据重新发布到 ZMQ：

```
.mcap 文件 → MCAP Reader → ZMQ PUB :5560 → 下游订阅者
```

| 参数 | 值 |
|---|---|
| 回放 ZMQ 端口 | 5560 |
| 速度范围 | 0.1x ~ 10.0x |
| 时间保持 | 按原始消息间隔回放，乘以速度系数 |
| 计算公式 | `sleep_ns = delta_ns / speed` |

---

## 8. 线程与调度模型

### 8.1 iPhone App 线程

| 队列名 | QoS | 职责 |
|---|---|---|
| Main Thread | - | UI 更新、状态变更 |
| `com.iphoneVIO.network` | userInitiated | TCP 数据发送/接收 |
| `com.iphoneVIO.jpeg` | userInitiated | JPEG 编码（避免阻塞 ARKit 回调） |
| `com.iphoneVIO.bonjour.advertise` | default | Bonjour 服务广播 |
| `com.iphoneVIO.bonjour.browse` | default | 浏览 `_vioserver._tcp` |
| `com.iphoneVIO.bonjour.rapiddriver` | default | 浏览 `_rapiddriver._tcp` |

### 8.2 树莓派引擎

| 组件 | 频率 | 说明 |
|---|---|---|
| 引擎主循环 | 500 Hz | 设备状态检查、mask 更新 |
| ZMQ SUB 接收 | 异步 tokio | 持续接收 msgpack 帧 |
| MCAP 写入 | 500 Hz（随主循环 tick） | 排空消息队列并写入 |
| Hardware Mask | 10 Hz | 50:1 降采样写入 MCAP |
| 心跳超时 | 3.0 秒 | 设备离线判定 |
| 客户端断开宽限 | 4.0 秒 | 防止瞬断误触发 |
| 启动宽限 | 6.0 秒 | 设备启动等待期 |

---

## 9. 关键源文件索引

### iPhone App (`/Users/yinzi/iPhoneVIO/`)

| 文件 | 职责 |
|---|---|
| `iPhoneVIO/NetworkClient.swift` | TCP 客户端，二进制帧封装与发送，NWConnection |
| `iPhoneVIO/BonjourManager.swift` | Bonjour 服务广播与发现 |
| `iPhoneVIO/ARSessionManager.swift` | ARKit 帧采集，JPEG 编码，位姿提取 |
| `iPhoneVIO/ContentView.swift` | 自动连接/重连逻辑 |
| `iPhoneVIO/RecordingController.swift` | HTTP API 调用，录制控制 |
| `iPhoneVIO/ARAction.swift` | 组件间事件通信（connect/disconnect/reset） |
| `iPhoneVIO/Info.plist` | Bonjour 服务声明，网络权限 |

### 树莓派 (`/Users/yinzi/mcap_iphone/`)

| 文件 | 职责 |
|---|---|
| `src/main.rs` | CLI 入口，clap 参数解析 |
| `src/engine/mod.rs` | 引擎核心，设备管理，iPhone 地址解析 |
| `src/engine/recorder/mod.rs` | RecorderCore，500Hz tick 处理 |
| `src/engine/recorder/zmq_sub.rs` | ZMQ SUB 接收 msgpack 帧 |
| `src/engine/recorder/messages.rs` | Msgpack 解析，JSON 转换，Base64 编码 |
| `src/engine/recorder/mcap_writer.rs` | MCAP 文件创建与写入 |
| `src/engine/recorder/schemas.rs` | JSON Schema 定义（foxglove/zumi） |
| `src/engine/recording.rs` | 录制会话管理，环境变量注入 |
| `src/engine/replay.rs` | MCAP 回放，ZMQ PUB 发布 |
| `src/discovery/mdns.rs` | mDNS 设备发现 |
| `src/api/routes.rs` | HTTP REST API 路由 |
