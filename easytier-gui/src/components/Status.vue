<script setup lang="ts">
import type { NodeInfo, PeerRoutePair } from '~/types/network'

const props = defineProps<{
  instanceId?: string
}>()

const { t } = useI18n()

const networkStore = useNetworkStore()

const curNetwork = computed(() => {
  if (props.instanceId) {
    // console.log('instanceId', props.instanceId)
    const c = networkStore.networkList.find(n => n.instance_id === props.instanceId)
    if (c !== undefined)
      return c
  }

  return networkStore.curNetwork
})

const curNetworkInst = computed(() => {
  return networkStore.networkInstances.find(n => n.instance_id === curNetwork.value.instance_id)
})

const peerRouteInfos = computed(() => {
  if (curNetworkInst.value) {
    const my_node_info = curNetworkInst.value.detail?.my_node_info
    return [{
      route: {
        ipv4_addr: my_node_info?.virtual_ipv4,
        hostname: my_node_info?.hostname,
        version: my_node_info?.version,
      },
    }, ...(curNetworkInst.value.detail?.peer_route_pairs || [])]
  }

  return []
})

function routeCost(info: any) {
  if (!info.peer)
    return t('status.local')

  if (info.route) {
    const cost = info.route.cost
    return cost === 1 ? 'p2p' : `relay(${cost})`
  }
  return '?'
}

function resolveObjPath(path: string, obj = globalThis, separator = '.') {
  const properties = Array.isArray(path) ? path : path.split(separator)
  return properties.reduce((prev, curr) => prev?.[curr], obj)
}

function statsCommon(info: any, field: string): number | undefined {
  if (!info.peer)
    return undefined

  const conns = info.peer.conns
  return conns.reduce((acc: number, conn: any) => {
    return acc + resolveObjPath(field, conn)
  }, 0)
}

function humanFileSize(bytes: number, si = false, dp = 1) {
  const thresh = si ? 1000 : 1024

  if (Math.abs(bytes) < thresh)
    return `${bytes} B`

  const units = si
    ? ['kB', 'MB', 'GB', 'TB', 'PB', 'EB', 'ZB', 'YB']
    : ['KiB', 'MiB', 'GiB', 'TiB', 'PiB', 'EiB', 'ZiB', 'YiB']
  let u = -1
  const r = 10 ** dp

  do {
    bytes /= thresh
    ++u
  } while (Math.round(Math.abs(bytes) * r) / r >= thresh && u < units.length - 1)

  return `${bytes.toFixed(dp)} ${units[u]}`
}

function latencyMs(info: PeerRoutePair) {
  let lat_us_sum = statsCommon(info, 'stats.latency_us')
  if (lat_us_sum === undefined)
    return ''
  lat_us_sum = lat_us_sum / 1000 / info.peer!.conns.length
  return `${lat_us_sum % 1 > 0 ? Math.round(lat_us_sum) + 1 : Math.round(lat_us_sum)}ms`
}

function txBytes(info: PeerRoutePair) {
  const tx = statsCommon(info, 'stats.tx_bytes')
  return tx ? humanFileSize(tx) : ''
}

function rxBytes(info: PeerRoutePair) {
  const rx = statsCommon(info, 'stats.rx_bytes')
  return rx ? humanFileSize(rx) : ''
}

function lossRate(info: PeerRoutePair) {
  const lossRate = statsCommon(info, 'loss_rate')
  return lossRate !== undefined ? `${Math.round(lossRate * 100)}%` : ''
}

function version(info: PeerRoutePair) {
  return info.route.version === '' ? 'unknown' : info.route.version
}

const myNodeInfo = computed(() => {
  if (!curNetworkInst.value)
    return {} as NodeInfo

  return curNetworkInst.value.detail?.my_node_info
})

interface Chip {
  label: string
  icon: string
}

const myNodeInfoChips = computed(() => {
  if (!curNetworkInst.value)
    return []

  const chips: Array<Chip> = []
  const my_node_info = curNetworkInst.value.detail?.my_node_info
  if (!my_node_info)
    return chips

  // virtual ipv4

  chips.push({
    label: `Virtual IPv4: ${my_node_info.virtual_ipv4}`,
    icon: '',
  } as Chip)

  // local ipv4s
  const local_ipv4s = my_node_info.ips?.interface_ipv4s
  for (const [idx, ip] of local_ipv4s?.entries()) {
    chips.push({
      label: `Local IPv4 ${idx}: ${ip}`,
      icon: '',
    } as Chip)
  }

  // local ipv6s
  const local_ipv6s = my_node_info.ips?.interface_ipv6s
  for (const [idx, ip] of local_ipv6s?.entries()) {
    chips.push({
      label: `Local IPv6 ${idx}: ${ip}`,
      icon: '',
    } as Chip)
  }

  // public ip
  const public_ip = my_node_info.ips?.public_ipv4
  if (public_ip) {
    chips.push({
      label: `Public IP: ${public_ip}`,
      icon: '',
    } as Chip)
  }

  // listeners:
  const listeners = my_node_info.listeners
  for (const [idx, listener] of listeners?.entries()) {
    chips.push({
      label: `Listener ${idx}: ${listener}`,
      icon: '',
    } as Chip)
  }

  // udp nat type
  enum NatType {
    // has NAT; but own a single public IP, port is not changed
    Unknown = 0,
    OpenInternet = 1,
    NoPAT = 2,
    FullCone = 3,
    Restricted = 4,
    PortRestricted = 5,
    Symmetric = 6,
    SymUdpFirewall = 7,
  };
  const udpNatType: NatType = my_node_info.stun_info?.udp_nat_type
  if (udpNatType !== undefined) {
    const udpNatTypeStrMap = {
      [NatType.Unknown]: 'Unknown',
      [NatType.OpenInternet]: 'Open Internet',
      [NatType.NoPAT]: 'No PAT',
      [NatType.FullCone]: 'Full Cone',
      [NatType.Restricted]: 'Restricted',
      [NatType.PortRestricted]: 'Port Restricted',
      [NatType.Symmetric]: 'Symmetric',
      [NatType.SymUdpFirewall]: 'Symmetric UDP Firewall',
    }

    chips.push({
      label: `UDP NAT Type: ${udpNatTypeStrMap[udpNatType]}`,
      icon: '',
    } as Chip)
  }

  return chips
})

function globalSumCommon(field: string) {
  let sum = 0
  if (!peerRouteInfos.value)
    return sum

  for (const info of peerRouteInfos.value) {
    const tx = statsCommon(info, field)
    if (tx)
      sum += tx
  }
  return sum
}

function txGlobalSum() {
  return globalSumCommon('stats.tx_bytes')
}

function rxGlobalSum() {
  return globalSumCommon('stats.rx_bytes')
}

const peerCount = computed(() => {
  if (!peerRouteInfos.value)
    return 0

  return peerRouteInfos.value.length
})

// calculate tx/rx rate every 2 seconds
let rateIntervalId = 0
const rateInterval = 2000
let prevTxSum = 0
let prevRxSum = 0
const txRate = ref('0')
const rxRate = ref('0')
onMounted(() => {
  rateIntervalId = window.setInterval(() => {
    const curTxSum = txGlobalSum()
    txRate.value = humanFileSize((curTxSum - prevTxSum) / (rateInterval / 1000))
    prevTxSum = curTxSum

    const curRxSum = rxGlobalSum()
    rxRate.value = humanFileSize((curRxSum - prevRxSum) / (rateInterval / 1000))
    prevRxSum = curRxSum
  }, rateInterval)
})

onUnmounted(() => {
  clearInterval(rateIntervalId)
})

const dialogVisible = ref(false)
const dialogContent = ref<any>('')
const dialogHeader = ref('event_log')

function showVpnPortalConfig() {
  const my_node_info = myNodeInfo.value
  if (!my_node_info)
    return

  const url = 'https://www.wireguardconfig.com/qrcode'
  dialogContent.value = `${my_node_info.vpn_portal_cfg}\n\n # can generate QR code: ${url}`
  dialogHeader.value = 'vpn_portal_config'
  dialogVisible.value = true
}

function showEventLogs() {
  const detail = curNetworkInst.value?.detail
  if (!detail)
    return

  dialogContent.value = detail.events
  dialogHeader.value = 'event_log'
  dialogVisible.value = true
}
</script>

<template>
  <div>
    <Dialog v-model:visible="dialogVisible" modal :header="t(dialogHeader)" :style="{ width: '70%' }">
      <Panel>
        <ScrollPanel style="width: 100%; height: 400px">
          <pre>{{ dialogContent }}</pre>
        </ScrollPanel>
      </Panel>
      <Divider />
      <div class="flex justify-content-end gap-2">
        <Button type="button" :label="t('close')" @click="dialogVisible = false" />
      </div>
    </Dialog>

    <Card v-if="curNetworkInst?.error_msg">
      <template #title>
        Run Network Error
      </template>
      <template #content>
        <div class="flex flex-column gap-y-5">
          <div class="text-red-500">
            {{ curNetworkInst.error_msg }}
          </div>
        </div>
      </template>
    </Card>

    <template v-else>
      <Card>
        <template #title>
          {{ t('my_node_info') }}
        </template>
        <template #content>
          <div class="flex w-full flex-column gap-y-5">
            <div class="m-0 flex flex-row justify-center gap-x-5">
              <div
                class="rounded-full w-32 h-32 flex flex-column align-items-center pt-4"
                style="border: 1px solid green"
              >
                <div class="font-bold">
                  {{ t('peer_count') }}
                </div>
                <div class="text-5xl mt-1">
                  {{ peerCount }}
                </div>
              </div>

              <div
                class="rounded-full w-32 h-32 flex flex-column align-items-center pt-4"
                style="border: 1px solid purple"
              >
                <div class="font-bold">
                  {{ t('upload') }}
                </div>
                <div class="text-xl mt-2">
                  {{ txRate }}/s
                </div>
              </div>

              <div
                class="rounded-full w-32 h-32 flex flex-column align-items-center pt-4"
                style="border: 1px solid fuchsia"
              >
                <div class="font-bold">
                  {{ t('download') }}
                </div>
                <div class="text-xl mt-2">
                  {{ rxRate }}/s
                </div>
              </div>
            </div>

            <div class="flex flex-row align-items-center flex-wrap w-full max-h-40 overflow-scroll">
              <Chip
                v-for="(chip, i) in myNodeInfoChips" :key="i" :label="chip.label" :icon="chip.icon"
                class="mr-2 mt-2 text-sm"
              />
            </div>

            <div v-if="myNodeInfo" class="m-0 flex flex-row justify-center gap-x-5 text-sm">
              <Button severity="info" :label="t('show_vpn_portal_config')" @click="showVpnPortalConfig" />
              <Button severity="info" :label="t('show_event_log')" @click="showEventLogs" />
            </div>
          </div>
        </template>
      </Card>

      <Divider />

      <Card>
        <template #title>
          {{ t('peer_info') }}
        </template>
        <template #content>
          <DataTable :value="peerRouteInfos" column-resize-mode="fit" table-style="width: 100%">
            <Column field="route.ipv4_addr" style="width: 100px;" :header="t('virtual_ipv4')" />
            <Column field="route.hostname" style="max-width: 250px;" :header="t('hostname')" />
            <Column :field="routeCost" style="width: 100px;" :header="t('route_cost')" />
            <Column :field="latencyMs" style="width: 80px;" :header="t('latency')" />
            <Column :field="txBytes" style="width: 80px;" :header="t('upload_bytes')" />
            <Column :field="rxBytes" style="width: 80px;" :header="t('download_bytes')" />
            <Column :field="lossRate" style="width: 100px;" :header="t('loss_rate')" />
            <Column :field="version" style="width: 100px;" :header="t('status.version')" />
          </DataTable>
        </template>
      </Card>
    </template>
  </div>
</template>
