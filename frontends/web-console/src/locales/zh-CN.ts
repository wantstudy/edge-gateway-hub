export default {
  app: {
    name: 'IoT-DAQ 网关控制台',
    desc: '工业边缘数据采集与统一分发平台'
  },
  common: {
    save: '保存',
    cancel: '取消',
    confirm: '确认',
    delete: '删除',
    edit: '编辑',
    add: '添加',
    search: '搜索',
    refresh: '刷新',
    loading: '加载中...',
    noData: '暂无数据',
    success: '操作成功',
    error: '操作失败',
    confirmDelete: '确认删除？',
    confirmReset: '确认重置？'
  },
  menu: {
    overview: '概览',
    devices: '设备管理',
    points: '点位管理',
    northbound: '北向转发',
    rules: '告警规则',
    license: '授权管理',
    monitor: '实时监控',
    audit: '审计日志',
    settings: '系统设置',
    startup: '自启动'
  },
  license: {
    trial: '试用版',
    expired: '已过期',
    active: '已激活',
    daysLeft: '剩余 {n} 天',
    northForward: '北向转发'
  },
  device: {
    name: '设备名称',
    protocol: '协议类型',
    status: '状态',
    online: '在线',
    offline: '离线',
    lastSeen: '最后活跃'
  },
  point: {
    name: '点位名称',
    value: '当前值',
    quality: '质量',
    good: '正常',
    bad: '异常',
    timestamp: '时间戳'
  },
  northbound: {
    brokerUrl: 'Broker 地址',
    topic: '主题',
    encoding: '编码方式',
    protobuf: 'Protobuf',
    json: 'JSON'
  },
  alarm: {
    name: '告警名称',
    level: '级别',
    info: '信息',
    warning: '警告',
    critical: '严重',
    triggered: '已触发',
    ack: '已确认'
  }
}
