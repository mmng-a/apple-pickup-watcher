import { type ReactNode, useEffect, useMemo, useState, useSyncExternalStore } from "react";
import {
  AlertTriangle,
  BellRing,
  Download,
  Pause,
  Play,
  Plus,
  RefreshCw,
  Settings2,
  Trash2,
  X,
} from "lucide-react";

import { Combobox } from "@/components/Combobox";
import { LogFooter } from "@/components/LogFooter";
import { MultiCombobox } from "@/components/MultiCombobox";
import { WatchList } from "@/components/WatchList";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  Sheet,
  SheetBody,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { Switch } from "@/components/ui/switch";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { groupByProduct, inAddedOrder, latestCheck } from "@/lib/groups";
import {
  capacityOptions,
  colorOptions,
  familyOptions,
  keepAvailable,
  productsForSelection,
} from "@/lib/product-selection";
import {
  cleanPushRows,
  PUSH_KIND_LABEL,
  type PushKind,
  samePushUrls,
  savedPushAddresses,
} from "@/lib/push";
import {
  changeLocale,
  connect,
  dismissUpdate,
  installUpdate,
  refreshProducts,
  saveSettings,
  setCategory,
  setIntervalSeconds,
  setPushUrls,
  setTargets,
  startWatching,
  stopWatching,
  testNotify,
  watcherStore,
} from "@/lib/store";
import {
  type Category,
  DEFAULT_MONITORING_HOURS,
  type MonitoringHours,
  describeAdvice,
  formatTime,
  isUntrusted,
  type Product,
  type Target,
  targetKey,
} from "@/lib/types";
import { cn } from "@/lib/utils";

/** 运行状态。一个会呼吸的点比一行灰字更像「它还活着」。 */
function RunState({ running, waiting }: { running: boolean; waiting: boolean }) {
  const active = running && !waiting;
  return (
    <span className="flex shrink-0 items-center gap-2 text-sm font-medium">
      <span className="relative flex size-2">
        {active && (
          <span className="bg-in-stock/60 absolute inline-flex size-full animate-ping rounded-full [animation-duration:2.4s]" />
        )}
        <span
          className={cn(
            "relative inline-flex size-2 rounded-full",
            active ? "bg-in-stock" : "bg-muted-foreground/40",
          )}
        />
      </span>
      {waiting ? "監視時間待ち" : running ? "監視中" : "一時停止中"}
    </span>
  );
}

/** 添加面板里的一行：左边标签，右边控件。 */
function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="grid grid-cols-[3rem_minmax(0,1fr)] items-center gap-3">
      <Label className="text-muted-foreground font-normal">{label}</Label>
      {children}
    </div>
  );
}

/** 设置面板里的一节。 */
function SettingsSection({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="grid gap-3">
      <h3 className="text-muted-foreground text-xs font-medium">{title}</h3>
      {children}
    </section>
  );
}

/**
 * 面板打开时焦点落在面板本身，不自动进第一个输入框：设置面板一打开就亮着
 * 「查询间隔」的焦点框，像是在催人改它。按 Tab 照常进入第一个控件。
 */
function focusPanel(event: Event) {
  event.preventDefault();
  (event.currentTarget as HTMLElement | null)?.focus();
}

export default function App() {
  const ui = useSyncExternalStore(watcherStore.subscribe, watcherStore.getSnapshot);

  useEffect(() => {
    void connect();
    // 刻意不在清理函数里断开：这是应用级的单一连接，窗口活着它就该活着。
    // StrictMode 的重复调用由 connect 内部去重。
  }, []);

  // 主窗口只放监控结果。添加和设置是从右侧滑出的面板，用完就收：它们一直摆在
  // 上面的时候，默认窗口里的监控列表只剩三分之一的高度。
  const [sheet, setSheet] = useState<"add" | "settings" | null>(null);
  // 门店可以一次选好几家：同城门店合并成一次请求之后，多选不增加请求量。
  const [storeNumbers, setStoreNumbers] = useState<string[]>([]);
  // 非 iPhone 品类直接选一个完整型号。
  const [partNumber, setPartNumber] = useState("");
  // iPhone 按机型 → 容量 → 颜色三级选择，三级都可多选：发售当晚常见的是
  // 「这两款、这三个颜色，哪个有货要哪个」，一台一台加太慢。三级组合里目录中
  // 存在的型号全部加入。
  const [families, setFamilies] = useState<string[]>([]);
  const [capacities, setCapacities] = useState<string[]>([]);
  const [colors, setColors] = useState<string[]>([]);
  // 刚加进去几个。新的一组排在列表后面，可能被面板挡着或者要滚动才看得到，
  // 面板底部说一声。
  const [justAdded, setJustAdded] = useState(0);
  // 推送地址：Bark 和飞书是同一个列表，一行一个。null 表示还没动过，跟着后端走；
  // 动过之后以这份为准，保持用户填写的顺序，渠道标签按后端的归类显示。
  const [pushDraft, setPushDraft] = useState<string[] | null>(null);
  // 代理地址和推送地址一样一行一个，草稿的规矩也相同，见 pushDraft。
  const [proxyDraft, setProxyDraft] = useState<string[] | null>(null);
  const [intervalDraft, setIntervalDraft] = useState<number | null>(null);

  function openAdd() {
    setJustAdded(0);
    setSheet("add");
  }

  const savedPush = useMemo(() => savedPushAddresses(ui.settings), [ui.settings]);
  const savedPushKind = new Map<string, PushKind>(savedPush.map((p) => [p.url, p.kind]));
  const pushRows = pushDraft ?? savedPush.map((p) => p.url);
  // 一个地址都没有时留一个空行，直接粘贴就行，不用先点「添加」。
  const visiblePushRows = pushRows.length > 0 ? pushRows : [""];
  const lastPushRow = visiblePushRows[visiblePushRows.length - 1] ?? "";

  function editPushRow(index: number, value: string) {
    setPushDraft(visiblePushRows.map((row, i) => (i === index ? value : row)));
  }

  // 失焦和删除都走这里：整理后整组交给后端归类保存；没有实际改动就不写盘。
  // 整理后的列表留作草稿：保存是异步的，这时退回「跟着后端走」，界面会先闪回
  // 旧地址，紧接着点「添加」还会以旧列表为底，把刚填的地址弄丢。
  function commitPushRows(rows: string[]) {
    const urls = cleanPushRows(rows);
    setPushDraft(urls);
    if (!samePushUrls(urls, savedPush.map((p) => p.url))) {
      void setPushUrls(urls);
    }
  }
  // 旧设置文件没有 proxies 字段，读上来是 undefined，当作空列表。
  const savedProxies = ui.settings.proxies ?? [];
  const proxyRows = proxyDraft ?? savedProxies;
  const visibleProxyRows = proxyRows.length > 0 ? proxyRows : [""];
  const lastProxyRow = visibleProxyRows[visibleProxyRows.length - 1] ?? "";

  function editProxyRow(index: number, value: string) {
    setProxyDraft(visibleProxyRows.map((row, i) => (i === index ? value : row)));
  }

  // 后端只保留能解析的代理地址；被丢弃的行需标为「未生效」。
  function commitProxyRows(rows: string[]) {
    const proxies = cleanPushRows(rows);
    setProxyDraft(proxies);
    if (!samePushUrls(proxies, savedProxies)) {
      void saveSettings({ ...ui.settings, proxies });
    }
  }
  const intervalValue = intervalDraft ?? ui.settings.intervalSeconds;
  const [hoursDraft, setHoursDraft] = useState<MonitoringHours | null>(null);
  const hours = hoursDraft ?? ui.settings.monitoringHours ?? DEFAULT_MONITORING_HOURS;
  const allDay = hours.startHour === hours.endHour % 24;
  const hoursDescription = allDay
    ? "終日監視"
    : `${hours.startHour}時〜${hours.startHour > hours.endHour ? "翌" : ""}${hours.endHour}時に監視`;

  const storeOptions = useMemo(
    () => ui.stores.map((s) => ({ value: s.number, label: s.title })),
    [ui.stores],
  );
  const storeIndex = useMemo(() => new Map(ui.stores.map((s) => [s.number, s])), [ui.stores]);
  // 只列当前品类。四个品类的型号加起来好几百条，全堆进一个下拉框，
  // 想找一台 Mac 得先划过所有 iPhone。
  const productOptions = useMemo(
    () =>
      ui.products
        .filter((p) => p.category === ui.category)
        .map((p) => ({ value: p.partNumber, label: p.title })),
    [ui.products, ui.category],
  );
  const iphoneFamilyOptions = useMemo(
    () => familyOptions(ui.products, ui.category),
    [ui.products, ui.category],
  );
  const iphoneCapacityOptions = useMemo(
    () => capacityOptions(ui.products, ui.category, families),
    [ui.products, ui.category, families],
  );
  const iphoneColorOptions = useMemo(
    () => colorOptions(ui.products, ui.category, families, capacities),
    [ui.products, ui.category, families, capacities],
  );
  // 点「添加」时会加入的型号。iPhone 是三级选择的组合，其他品类就是选中的那一个。
  const selectedProducts = useMemo<Product[]>(() => {
    if (ui.category === "iphone") {
      return productsForSelection(ui.products, ui.category, families, capacities, colors);
    }
    const product = ui.products.find((p) => p.partNumber === partNumber);
    return product ? [product] : [];
  }, [ui.products, ui.category, families, capacities, colors, partNumber]);
  const selectedStores = ui.stores.filter((s) => storeNumbers.includes(s.number));

  function resetProductSelection() {
    setPartNumber("");
    setFamilies([]);
    setCapacities([]);
    setColors([]);
  }

  // 上级变了，下级只去掉不再可选的项，仍然成立的留着（理由见 keepAvailable）。
  function onFamiliesChange(next: string[]) {
    setFamilies(next);
    const nextCapacities = keepAvailable(
      capacities,
      capacityOptions(ui.products, ui.category, next),
    );
    setCapacities(nextCapacities);
    setColors(
      keepAvailable(colors, colorOptions(ui.products, ui.category, next, nextCapacities)),
    );
  }

  function onCapacitiesChange(next: string[]) {
    setCapacities(next);
    setColors(keepAvailable(colors, colorOptions(ui.products, ui.category, families, next)));
  }

  // 添加、删除都在这份上改，再整份交给后端：按加入的先后排，列表里各组的位置才不会变。
  const targets = useMemo(
    () => inAddedOrder(ui.rows, ui.settings.targets),
    [ui.rows, ui.settings.targets],
  );
  const groups = useMemo(
    () => groupByProduct(ui.rows, ui.settings.targets),
    [ui.rows, ui.settings.targets],
  );
  // 列表里不止一个地区时，组标题旁标出地区：同一个型号名可能在两个地区各有一组。
  const regionTitle = useMemo(() => {
    const locales = new Set(ui.rows.map((r) => r.target.locale));
    if (locales.size < 2) return null;
    return (locale: string) => ui.regions.find((r) => r.locale === locale)?.title ?? locale;
  }, [ui.rows, ui.regions]);

  const summary = useMemo(() => {
    let inStock = 0;
    let untrusted = 0;
    for (const r of ui.rows) {
      if (r.availability.kind === "in_stock") inStock += 1;
      if (isUntrusted(r.availability)) untrusted += 1;
    }
    return { inStock, untrusted, lastChecked: latestCheck(ui.rows) };
  }, [ui.rows]);

  // 门店 × 型号，每个组合一条目标；已经在列表里的跳过，不重复。
  const newTargets = useMemo(() => {
    const existing = new Set(targets.map(targetKey));
    const added: Target[] = [];
    for (const number of storeNumbers) {
      const store = ui.stores.find((s) => s.number === number);
      if (!store) continue;
      for (const product of selectedProducts) {
        const next: Target = {
          locale: ui.settings.locale,
          storeNumber: store.number,
          storeTitle: store.title,
          partNumber: product.partNumber,
          productName: product.title,
          ...(product.companionPart ? { companionPart: product.companionPart } : {}),
        };
        const key = targetKey(next);
        if (existing.has(key)) continue;
        existing.add(key);
        added.push(next);
      }
    }
    return added;
  }, [targets, storeNumbers, selectedProducts, ui.stores, ui.settings.locale]);
  const combos = storeNumbers.length * selectedProducts.length;

  async function onAdd() {
    if (newTargets.length === 0) return;
    const added = newTargets;
    await setTargets([...targets, ...added]);
    // 只清型号，门店留着，面板也不关：接着给同一批门店加下一个型号是最常见的操作。
    resetProductSelection();
    setJustAdded(added.length);
  }

  async function onRemove(remove: Target[]) {
    const keys = new Set(remove.map(targetKey));
    await setTargets(targets.filter((t) => !keys.has(targetKey(t))));
  }

  // 顶栏的一行状态：怎么在查、上一轮什么时候、查出了什么。
  const status: { text: string; className?: string }[] = [];
  if (ui.running && !ui.waitingForSchedule) {
    status.push({
      text: `${ui.settings.intervalSeconds}秒ごとに確認`,
    });
  }
  if (ui.waitingForSchedule) {
    status.push({ text: `時間外のため待機中（日本時間 ${hoursDescription}）` });
  }
  if (summary.lastChecked !== null) {
    status.push({ text: `最終確認 ${formatTime(summary.lastChecked)}` });
  }
  if (ui.rows.length > 0) status.push({ text: `監視対象 ${ui.rows.length}件` });
  if (summary.inStock > 0) {
    status.push({ text: `在庫あり ${summary.inStock}件`, className: "text-in-stock font-medium" });
  }
  if (summary.untrusted > 0) {
    // 「其中多少项查不到」一直点出来：这个数字大于 0 时，
    // 列表里那些「无货」也未必反映真实情况。
    status.push({ text: `確認不可 ${summary.untrusted}件`, className: "text-unknown font-medium" });
  }

  return (
    <TooltipProvider delayDuration={200}>
      <div className="mx-auto flex h-screen max-w-6xl flex-col">
        <header className="flex items-center gap-3 px-6 pt-4 pb-3">
          <RunState running={ui.running} waiting={ui.waitingForSchedule} />
          <p
            className="text-muted-foreground min-w-0 flex-1 truncate text-sm"
            title={status.map((s) => s.text).join(" · ")}
          >
            {status.map((s, i) => (
              <span key={s.text}>
                {i > 0 && " · "}
                <span className={s.className}>{s.text}</span>
              </span>
            ))}
          </p>
          <Button variant="ghost" onClick={openAdd}>
            <Plus /> 追加
          </Button>
          <Button variant="ghost" onClick={() => setSheet("settings")}>
            <Settings2 /> 設定
          </Button>
          {ui.running ? (
            <Button variant="secondary" onClick={() => void stopWatching()}>
              <Pause /> 一時停止
            </Button>
          ) : (
            <Button onClick={() => void startWatching()} disabled={ui.rows.length === 0}>
              <Play /> 開始
            </Button>
          )}
        </header>

        {(ui.trouble !== null || ui.update !== null) && (
          <div className="grid gap-2 px-6 pb-3">
            {ui.trouble !== null && (
              // 只有标题是红的。原因和建议各占一行、用正常的字色：三段红字堆在一起，
              // 用户反而分不清哪句是原因、哪句是该做的事。
              <div
                role="alert"
                className="border-destructive/30 bg-destructive/5 border-l-destructive grid gap-1.5 rounded-lg border border-l-4 px-4 py-3 text-sm"
              >
                <p className="text-destructive flex flex-wrap items-center gap-x-2 font-medium">
                  <AlertTriangle className="size-4" />
                  在庫情報を確認できません
                  <span className="text-muted-foreground font-normal">
                    復旧するまで、一覧の表示は店舗の実際の在庫を反映しません
                  </span>
                </p>
                <p className="pl-6">{ui.trouble.reason}</p>
                {ui.trouble.advice !== null && (
                  // 用户自己能做的那件事单独拎出来，而且要短：issue #3 里那位对着反复
                  // 告警的窗口干等了三个小时，缺的不是解释，是一句「换条网络」。
                  <p className="text-muted-foreground pl-6">
                    <span className="text-foreground font-medium">対処方法</span>{" "}
                    {describeAdvice(ui.trouble.advice)}
                  </p>
                )}
              </div>
            )}

            {ui.update !== null && (
              // 只提示，不自作主张安装。用户可能正等着抢购，被强制重启是灾难。
              <div className="bg-card flex flex-wrap items-center gap-x-4 gap-y-2 rounded-lg border px-4 py-2.5 text-sm shadow-xs">
                <p className="flex items-center gap-2 font-medium">
                  <Download className="size-4" /> 新しいバージョン {ui.update.version}
                </p>
                <p className="text-muted-foreground">
                  現在のバージョンは{ui.update.currentVersion}です。インストール後にアプリを再起動してください。
                </p>
                <div className="ml-auto flex items-center gap-2">
                  <Button size="sm" disabled={ui.installing} onClick={() => void installUpdate()}>
                    {ui.installing ? "ダウンロード中…" : "ダウンロードしてインストール"}
                  </Button>
                  <Button size="sm" variant="ghost" onClick={dismissUpdate}>
                    <X /> あとで
                  </Button>
                </div>
              </div>
            )}
          </div>
        )}

        {ui.rows.length === 0 ? (
          <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-3 pb-16">
            {ui.ready ? (
              <>
                <p className="text-muted-foreground text-sm">監視対象がありません</p>
                <Button onClick={openAdd}>
                  <Plus /> 監視対象を追加
                </Button>
              </>
            ) : (
              <p className="text-muted-foreground text-sm">読み込み中…</p>
            )}
          </div>
        ) : (
          <ScrollArea className="min-h-0 flex-1">
            <WatchList
              groups={groups}
              stores={storeIndex}
              regionTitle={regionTitle}
              onRemove={(remove) => void onRemove(remove)}
            />
          </ScrollArea>
        )}

        <LogFooter logs={ui.logs} />
      </div>

      <Sheet open={sheet === "add"} onOpenChange={(open) => setSheet(open ? "add" : null)}>
        <SheetContent aria-describedby={undefined} onOpenAutoFocus={focusPanel}>
          <SheetHeader>
            <SheetTitle>監視対象を追加</SheetTitle>
          </SheetHeader>
          <SheetBody className="grid content-start gap-3">
            <Field label="地域">
              <Combobox
                className="w-full"
                options={ui.regions.map((r) => ({ value: r.locale, label: r.title }))}
                value={ui.settings.locale}
                onChange={(locale) => {
                  // 换地区后旧的门店和型号都不再适用，清掉待添加的选择。
                  setStoreNumbers([]);
                  resetProductSelection();
                  void changeLocale(locale);
                }}
                placeholder="地域を選択"
                searchPlaceholder="地域を検索…"
                emptyText="該当する地域がありません"
              />
            </Field>
            <Field label="店舗">
              <MultiCombobox
                className="w-full"
                options={storeOptions}
                values={storeNumbers}
                onChange={setStoreNumbers}
                placeholder="受取店舗を選択"
                searchPlaceholder="店舗を検索…"
                emptyText="該当する店舗がありません"
                countLabel={(n) => `${n}店舗を選択中`}
                disabled={storeOptions.length === 0}
              />
            </Field>
            {selectedStores.length > 1 && (
              // 门店在面板收起后仍然保留，下次打开要能直接看到是哪几家；
              // 触发器上只放得下一个数字。
              <p
                className="text-muted-foreground -mt-1 line-clamp-2 pl-15 text-xs"
                title={selectedStores.map((s) => s.title).join("、")}
              >
                {selectedStores.map((s) => s.title).join("、")}
              </p>
            )}

            <div className="my-1 border-t" />

            <Field label="種類">
              <Combobox
                className="w-full"
                options={ui.categories.map((c) => ({ value: c.value, label: c.title }))}
                value={ui.category}
                onChange={(value) => {
                  // 换品类后旧的型号不再在下拉框里，清掉待添加的选择。门店不用清，
                  // 它跟品类无关。
                  resetProductSelection();
                  setCategory(value as Category);
                }}
                placeholder="製品カテゴリを選択"
                searchPlaceholder="製品カテゴリを検索…"
                emptyText="該当する製品カテゴリがありません"
                disabled={ui.categories.length === 0}
              />
            </Field>
            {ui.category === "iphone" ? (
              <>
                <Field label="モデル">
                  <MultiCombobox
                    className="w-full"
                    options={iphoneFamilyOptions}
                    values={families}
                    onChange={onFamiliesChange}
                    placeholder="モデルを選択"
                    searchPlaceholder="モデルを検索…"
                    emptyText="該当するモデルがありません"
                    countLabel={(n) => `${n}モデルを選択中`}
                    disabled={iphoneFamilyOptions.length === 0}
                  />
                </Field>
                <Field label="容量">
                  <MultiCombobox
                    className="w-full"
                    options={iphoneCapacityOptions}
                    values={capacities}
                    onChange={onCapacitiesChange}
                    placeholder="容量を選択"
                    searchPlaceholder="容量を検索…"
                    emptyText="該当する容量がありません"
                    countLabel={(n) => `${n}種類の容量を選択中`}
                    disabled={families.length === 0 || iphoneCapacityOptions.length === 0}
                  />
                </Field>
                <Field label="カラー">
                  <MultiCombobox
                    className="w-full"
                    options={iphoneColorOptions}
                    values={colors}
                    onChange={setColors}
                    placeholder="カラーを選択"
                    searchPlaceholder="カラーを検索…"
                    emptyText="該当するカラーがありません"
                    countLabel={(n) => `${n}色を選択中`}
                    disabled={capacities.length === 0 || iphoneColorOptions.length === 0}
                  />
                </Field>
              </>
            ) : (
              <Field label="構成">
                <Combobox
                  className="w-full"
                  options={productOptions}
                  value={partNumber}
                  onChange={setPartNumber}
                  placeholder="構成を選択"
                  searchPlaceholder="構成を検索…"
                  emptyText="該当する構成がありません"
                  disabled={productOptions.length === 0}
                />
              </Field>
            )}
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  variant="link"
                  size="sm"
                  className="text-muted-foreground hover:text-foreground ml-15 h-auto justify-self-start p-0 text-xs font-normal has-[>svg]:px-0 [&_svg]:size-3"
                  disabled={ui.refreshing}
                  onClick={() => void refreshProducts()}
                >
                  <RefreshCw className={ui.refreshing ? "animate-spin" : undefined} />
                  製品一覧を更新
                </Button>
              </TooltipTrigger>
              <TooltipContent>
                Apple公式サイトから選択中のカテゴリの製品一覧を取得します。新製品の追加時もアプリの更新を待たずに利用できます。
              </TooltipContent>
            </Tooltip>
          </SheetBody>
          <SheetFooter>
            <p className="text-muted-foreground min-w-0 flex-1 text-sm">
              {combos > 0 ? (
                // 多选时把要加的规模点明：三级各勾几项之后，组合数并不直观。
                <span title={selectedProducts.map((p) => p.title).join("、")}>
                  {selectedProducts.length}構成 × {storeNumbers.length}店舗
                  {combos > newTargets.length
                    ? newTargets.length === 0
                      ? "（すべて監視対象に追加済み）"
                      : `（うち${combos - newTargets.length}件は追加済み）`
                    : null}
                </span>
              ) : justAdded > 0 ? (
                `${justAdded}件を追加しました`
              ) : null}
            </p>
            <Button onClick={() => void onAdd()} disabled={newTargets.length === 0}>
              <Plus /> 追加
            </Button>
          </SheetFooter>
        </SheetContent>
      </Sheet>

      <Sheet
        open={sheet === "settings"}
        onOpenChange={(open) => setSheet(open ? "settings" : null)}
      >
        <SheetContent onOpenAutoFocus={focusPanel}>
          <SheetHeader>
            <SheetTitle>設定</SheetTitle>
            <SheetDescription>変更は自動で保存されます</SheetDescription>
          </SheetHeader>
          <SheetBody className="grid content-start gap-7">
            <SettingsSection title="在庫確認">
              <div className="grid gap-2">
                <Label htmlFor="monitoring-start">監視時間（日本時間）</Label>
                <div
                  id="monitoring-hours"
                  className="flex items-center gap-2"
                  onBlur={(event) => {
                    if (event.currentTarget.contains(event.relatedTarget)) return;
                    setHoursDraft(null);
                    if (
                      Number.isInteger(hours.startHour) && hours.startHour >= 0 && hours.startHour < 24 &&
                      Number.isInteger(hours.endHour) && hours.endHour >= 0 && hours.endHour <= 24
                    ) {
                      void saveSettings({ ...ui.settings, monitoringHours: hours });
                    }
                  }}
                >
                  <Input
                    id="monitoring-start"
                    aria-label="監視開始時刻"
                    type="number"
                    min={0}
                    max={23}
                    step={1}
                    className="h-8 w-20 select-text"
                    value={Number.isFinite(hours.startHour) ? hours.startHour : ""}
                    onChange={(event) => setHoursDraft({ ...hours, startHour: event.target.valueAsNumber })}
                  />
                  <span className="text-muted-foreground text-sm">時〜</span>
                  <Input
                    id="monitoring-end"
                    aria-label="監視終了時刻"
                    type="number"
                    min={0}
                    max={24}
                    step={1}
                    className="h-8 w-20 select-text"
                    value={Number.isFinite(hours.endHour) ? hours.endHour : ""}
                    onChange={(event) => setHoursDraft({ ...hours, endHour: event.target.valueAsNumber })}
                  />
                  <span className="text-muted-foreground text-sm">時</span>
                </div>
                <p className="text-muted-foreground text-xs">{hoursDescription}。時間外は待機し、開始時刻になると自動で再開します。</p>
              </div>
              <div className="flex items-center justify-between gap-4">
                <Label htmlFor="interval">確認間隔</Label>
                <div className="flex items-center gap-2">
                  <Input
                    id="interval"
                    type="number"
                    min={5}
                    className="h-8 w-20 select-text"
                    value={intervalValue}
                    onChange={(e) => setIntervalDraft(e.target.valueAsNumber)}
                    onBlur={() => {
                      const s = Number.isFinite(intervalValue) ? Math.round(intervalValue) : 30;
                      setIntervalDraft(null);
                      void setIntervalSeconds(s);
                    }}
                  />
                  <span className="text-muted-foreground text-sm">秒</span>
                </div>
              </div>
            </SettingsSection>

            <SettingsSection title="通知">
              <div className="flex items-center justify-between gap-4">
                <Label htmlFor="sound">通知音</Label>
                <Switch
                  id="sound"
                  checked={ui.settings.soundEnabled}
                  onCheckedChange={(v) => void saveSettings({ ...ui.settings, soundEnabled: v })}
                />
              </div>
              <div className="flex items-center justify-between gap-4">
                <Label htmlFor="openbag">在庫があるときにバッグを開く</Label>
                <Switch
                  id="openbag"
                  checked={ui.settings.openBagOnHit}
                  onCheckedChange={(v) => void saveSettings({ ...ui.settings, openBagOnHit: v })}
                />
              </div>
              <div className="grid gap-1.5 pt-1">
                <div className="flex items-center gap-2">
                  <Label htmlFor="push-0" className="shrink-0">
                    通知先
                  </Label>
                  <span className="text-muted-foreground min-w-0 truncate text-xs">
                    BarkまたはFeishuグループのボット。1行に1件ずつ入力
                  </span>
                  {lastPushRow.trim() !== "" && (
                    <Button
                      variant="link"
                      size="sm"
                      className="text-muted-foreground hover:text-foreground ml-auto h-auto shrink-0 gap-1 p-0 text-xs has-[>svg]:px-0 [&_svg]:size-3"
                      onClick={() => setPushDraft([...cleanPushRows(visiblePushRows), ""])}
                    >
                      <Plus /> 通知先を追加
                    </Button>
                  )}
                </div>
                {visiblePushRows.map((row, index) => {
                  const kind = savedPushKind.get(row.trim());
                  return (
                    <div key={index} className="flex items-center gap-2">
                      {/* 渠道标签占固定宽度：还没保存的行留空，输入框照样对齐。 */}
                      <span className="flex w-12 shrink-0 justify-center">
                        {kind !== undefined && (
                          <Badge variant="outline" className="text-muted-foreground">
                            {PUSH_KIND_LABEL[kind]}
                          </Badge>
                        )}
                      </span>
                      <Input
                        id={`push-${index}`}
                        aria-label={`通知先 ${index + 1}`}
                        className="select-text"
                        placeholder="BarkのURL、またはFeishuグループのWebhook"
                        value={row}
                        // 只有点「添加」新开的那一行自动聚焦；打开面板时的空行不抢焦点。
                        autoFocus={pushDraft !== null && row === "" && index > 0}
                        onChange={(e) => editPushRow(index, e.target.value)}
                        onBlur={() => commitPushRows(visiblePushRows)}
                      />
                      <Button
                        variant="ghost"
                        size="icon"
                        aria-label={`通知先を削除 ${index + 1}`}
                        disabled={visiblePushRows.length === 1 && row === ""}
                        onClick={() =>
                          commitPushRows(visiblePushRows.filter((_, i) => i !== index))
                        }
                      >
                        <Trash2 />
                      </Button>
                    </div>
                  );
                })}
                {savedPush.some((p) => p.kind === "feishu") && (
                  // 飞书机器人最常见的配置错误是安全设置，只在真填了飞书地址时提醒。
                  <p className="text-muted-foreground pl-14 text-xs">
                    Feishuボットのセキュリティ設定で「カスタムキーワード」を選び、次を指定してください：「有货」。
                  </p>
                )}
              </div>
              {/* 测的就是上面这些：提示音、系统通知和推送一起发一遍。 */}
              <Button
                variant="outline"
                size="sm"
                className="justify-self-start"
                onClick={() => void testNotify()}
              >
                <BellRing /> 通知をテスト
              </Button>
            </SettingsSection>

            <SettingsSection title="ネットワーク">
              <div className="grid gap-1.5">
                <div className="flex items-center gap-2">
                  <Label htmlFor="proxy-0" className="shrink-0">
                    プロキシURL
                  </Label>
                  <span className="text-muted-foreground min-w-0 truncate text-xs">
                    http / https / socks5。1行に1件ずつ入力
                  </span>
                  {lastProxyRow.trim() !== "" && (
                    <Button
                      variant="link"
                      size="sm"
                      className="text-muted-foreground hover:text-foreground ml-auto h-auto shrink-0 gap-1 p-0 text-xs has-[>svg]:px-0 [&_svg]:size-3"
                      onClick={() => setProxyDraft([...cleanPushRows(visibleProxyRows), ""])}
                    >
                      <Plus /> プロキシを追加
                    </Button>
                  )}
                </div>
                {visibleProxyRows.map((row, index) => {
                  // 保存后后端没留下这一行：地址解析不了，这条线路不存在。
                  const rejected =
                    proxyDraft !== null && row.trim() !== "" && !savedProxies.includes(row.trim());
                  return (
                    <div key={index} className="flex items-center gap-2">
                      <Input
                        id={`proxy-${index}`}
                        aria-label={`プロキシURL ${index + 1}`}
                        aria-invalid={rejected || undefined}
                        className="select-text"
                        placeholder="socks5://127.0.0.1:1080 または http://host:port"
                        value={row}
                        autoFocus={proxyDraft !== null && row === "" && index > 0}
                        onChange={(e) => editProxyRow(index, e.target.value)}
                        onBlur={() => commitProxyRows(visibleProxyRows)}
                      />
                      {rejected && (
                        <Badge
                          variant="outline"
                          className="text-destructive border-destructive/40 shrink-0"
                          title="URLの形式が正しくないため、この接続先は使用されません。http、https、socks5、socks5hに対応しています。例：socks5://127.0.0.1:1080"
                        >
                          未適用
                        </Badge>
                      )}
                      <Button
                        variant="ghost"
                        size="icon"
                        aria-label={`プロキシURLを削除 ${index + 1}`}
                        disabled={visibleProxyRows.length === 1 && row === ""}
                        onClick={() =>
                          commitProxyRows(visibleProxyRows.filter((_, i) => i !== index))
                        }
                      >
                        <Trash2 />
                      </Button>
                    </div>
                  );
                })}
              </div>
            </SettingsSection>
          </SheetBody>
        </SheetContent>
      </Sheet>
    </TooltipProvider>
  );
}
