- [tcpm](#tcpm)
- [Usage](#usage)
- [项目笔记](#项目笔记)
    - [使用的工具](#使用的工具)
    - [项目结构](#项目结构)
    - [TCP协议相关](#tcp协议相关)
        - [TCP初始化阶段](#tcp初始化阶段)
        - [连接的被动打开](#连接的被动打开)
        - [通信过程](#通信过程)
            - [segment arrives](#segment-arrives)
            - [send call](#send-call)
            - [timer](#timer)
            - [close](#close)
            - [retransmit](#retransmit)
    - [接口设计](#接口设计)
        - [网卡接口](#网卡接口)
        - [TcpListener](#tcplistener)
        - [Read](#read)
        - [Write](#write)
        - [Shutdown](#shutdown)
    - [其它笔记](#其它笔记)
            - [header中序列号的循环计数问题](#header中序列号的循环计数问题)
            - [乱序重组](#乱序重组)
            - [Tips](#tips)


# tcpm
这是一个根据`RFC
793`文档实现的运行在用户态的`TCP`协议，使用的语言是`Rust`，目的只是为了学习`TCP`和`Rust`。
项目最初是fork自`Jonhoo`大佬的直播项目[rust-tcp](https://github.com/jonhoo/rust-tcp)，[Youtube](https://youtu.be/bzja9fQWzdA)上有直播录像。但是原项目还很不完善，只通过了一个特殊场景的测试，也还存在一些错误，然后大佬就弃坑了:joy:。

我在这基础上根据`RFC793`并参考[smoltcp](https://github.com/smoltcp-rs/smoltcp)重新写了`TCP`协议逻辑然后修正了一些其它东西。现在这个`TCP`实现在被动打开的情况下能正确工作，可以通过`wrk`的`HTTP`连接压测（需要另外实现一个简单的`HTTP
server`），也可以实现双向通信，也支持多个连接。不过在超低延迟（<1ms）情况下，由于架构设计的原因，`wrk`压测时超高频率的连接关闭和重连会引起错误（主要是还没发送自己的数据包就收到了新的`TCP`数据包，导致错误计算）。

顺便安利一下`Jonhoo`大佬的[Youtube](https://youtu.be/bzja9fQWzdA)频道，内容主要是`Rust`相关，极其硬核。他本人之前是`MIT`分布式系统课程`6.824`的助教，也是`MIT`
[MissingSemester](https://www.youtube.com/c/MissingSemester)的讲师之一。

# Usage
我们的程序依赖于`TUN/TAP`，需要一些额外的工作，所以我们使用脚本来辅助运行，就不直接提供二进制程序啦，感兴趣可以`clone`项目然后运行这个脚本即可。不过我只在`Linux`下测试，其它系统是否可用未知。
```
./run.sh
```
现在项目的`main.rs`中的代码实现了一个通信和echo服务，可以通过`netcat`命令连接，支持多个连接，效果如下。
<img src="https://s2.loli.net/2021/12/16/NlSgrFc7ZB4d6JP.gif" width=50%>

解释一下我们脚本做了什么：
1. 首先当然是使用`release`选项来编译。
2. 然后需要对我们编译后的程序赋予特殊权限，让我们在以普通身份运行它时可以操作网卡。具体可以查看文档:
   `man 7 capabilities`
3. 接下来给我们的虚拟网卡设置一个`IPv4`地址，并启用网卡。
4. 最后运行我们的程序。

请注意第三四步。赋予`IP`地址的操作需要一个网卡设备，这个设备只有我们的程序运行之后才会出现。为什么不交换三四步呢？因为交换之后我们的程序就不能获得`STDIN`了。所以第三步操作需要延迟一下，让它在我们的程序运行后再执行，可以使用`sleep`来实现异步执行。

如果不需要捕获`STDIN`，可以交换三四步，而且不用`sleep`，不过记得使用`trap`来捕获`ctrl
c`或者其它键盘信号，不然没法终止程序。

# 项目笔记
接下来写代码时的一些笔记~

## 使用的工具
1. 用`wireshark`来抓包，不过在刚开始的时候不太关心应用层的数据，所以直接用`cli`工具`tshark`来抓包。如果需要解析8进制的ip数据包，使用https://hpd.gasmi.net/
2. 使用`netcat`作为客户端来测试TCP连接过程。
3. 由于我们仅仅是为了学习`TCP`协议，并不关心网络数据包的捕获和发送方式，就不用`BPF`或者`DPDK`等工具了。我们使用的是[TUN/TAP](https://zh.wikipedia.org/wiki/TUN%E4%B8%8ETAP)，用它来创建虚拟网卡完成捕获和发送`TCP/IP`数据包，所以我们的项目是用户态`TCP`协议。
4. 同样，我们也不关心大小端、序列化和反序列化等`TCP/IP`数据包的解析，这个过程使用的是其它`etherparse`这个`Crate`。但是对于`TCP/IP
ader`的格式还是需要熟悉才行。
5. 我们使用了`poll`轮询来检查是否有新数据包，所以使用了`nix`这个`crate`。
6. 使用的一个简单的日志工具`log`和`simplelog`。
7. 为了实现一个双向通信的应用，需要用到`crossbeam_channel`。

这里有个比较奇怪的地方就是不能用调试工具，`LLDB`会导致我的`TUN/TAP`虚拟网卡退出，原因未知。所以只能`print
debug`啦，这就是为啥用了日志工具。

## 项目结构
在某个连接处于`Established`状态下的TCP流程示意图1所示：
<figure>
<img src="https://s2.loli.net/2021/12/17/aDh18bWXsd2UqRw.png" width=60%>
<figcaption>图1. `Establish`状态下的`packet`处理过程</figcaption>
</figure>

我们这里没有使用`channel`类型来做线程间通信。如果用`channel`的话，这是一个多生产者多消费者通道，可以使用`crossbeam`这个`crate`，不过这会导致多次数据的复制，所以这里使用了另外的处理方式。

此时我们有一个`Nic`网卡实例，这个实例中需要一个`packet_loop`无限循环来完成数据的持续收发，为了不阻塞我们的程序，这需要创建为一个单独的线程。我们将`TCP`连接相关的处理放在这里，那么这个循环会做三件事情：
- 我们有一个`ConnectionManager
  (CM)`来管理所有的连接，那么这个循环首先会检查`CM`中是否有标记为`removed`的连接，有的话将其从`CM`中销毁。
- 接下来会使用`poll`来轮询是否有数据抵达`nic`。如果没有，则会让`CM`中的每个连接调用`on_tick`方法发送或者重传需要的被发送的`TCP`数据包。
- 根据前面`poll`的结果，如果有数据到达并且是`TCP`数据，就将其**引用**保存到这个`socket`对应的连接的`receive
  space`，并根据`TCP`协议来调整连接的`TCB`，然后**唤醒**用户读取数据；如果没找到`socket`对应的连接，应该将这个`socket`放到一个等待队列中，然后**唤醒**`Listener`来创建一个新的连接。

在上一步中，每个连接相关的方法被调用时都需要获取`Nic`的锁，保证`TCP`处理过程中数据包本身没有改变。

然后为了让我们的`TCP`协议可用，我们需要实现简单的`socket`接口，我们这里只实现了`server`端的被动打开，所以至少需要`bind,
accept, read, write,
shutdown`几种方法。
1. `bind`：创建一个`Listener`，绑定在某个`nic`的端口；
2. `accept`：由`Listener`调用，创建新的连接。为了能够持续创建新的连接，这个`accept`内部是一个循环，不断从等待队列中拿取`socket`创建新连接并交给`ConnectionManager`。
3. `read`：在创建一个连接后，等待`packet_loop`唤醒，然后读取数据。数据的持续读出交给了应用层实现。
4. `shutdown`：主动或者被动关闭连接。由于我们没有实现`Listen`状态，所以连接进入`closed`状态后会被直接从`ConnectionManager`中删除。


接下来的内容是我完成本项目过程中的一些笔记。

## TCP协议相关

### TCP初始化阶段

首先需要建立一个用于监听网卡的接口`nic`，这时一个链路层的接口，它将会负责所有的网络数据包的接收和发送，其中自然也包括`tcp`连接，此时需要一个无限循环来保证`nic`的接收和发送的持续性，否则我们在接收一个数据包之后程序就会终止。

`nic`是一个链路层的接口，那么我们这个接口通信用到的是`ip`协议，我们这里不关心`ip`协议的细节，直接使用`etherparse`这个`rust crate`来提取或者封装得到的`tcp packet`。

这里需要注意`MTU`的限制，这里假设`MTU`为固定的`1500`字节，所以我们每次从`nic`读取数据包的长度便硬编码为`1500`字节。封装数据包的时候也要注意`MTU`检测每个`ip packet`的长度是否超过了`1500`字节，不过我们之后初始化`tcp`连接时会直接将我们的发送窗口硬编码为`1024`（当然依旧需要检查数据包的长度）。`MTU`和发送窗口在实际情况中大小可以是动态变化的，例如`TCP`协议的各种拥塞控制算法就是用来调整发送窗口大小的，不过这不是`RFC 793`解决的问题，所以之后我们的实现中也没有拥塞控制（注意滑动窗口和拥塞控制关心的不是同一件事情）。

`nic`中可能包含很多不同的`tcp连接`，可以使用`对方ip，对方端口和我们的端口`这样一个三元组来辨别不同的`tcp`连接。所以可以使用一个`HashMap`来保存这些连接，那么键值分别为三元组和`Transmission Control Block`。为了发送`tcp packet`，我们需要知道对方主机地址和端口号；为了正确读取`tcp packet`，我们还要知道这个`tcp packet`的目标端口号。所以为了使用方便，键的结构可以是一个包含两个`"socket"`的元组：`((client addr, client port), (our addr, our port))`，这些信息可以在`ip header`和`tcp header`中获取。


### 连接的被动打开

1. 当我们收到一个`tcp
packet`时（收到握手请求），需要判断这个一连接是否是新的连接还是已经存在的连接，如果是新的连接，那么需要在保存连接的`HashMap`中建立一个新的连接，即实现一个`establish`的方法。

    首先读取`tcp header`中`Control
Bits`里的`SYN`是否为`1`，如果不是的话就直接忽略这个`packet`。
   1.1 如果上一步`SYN`为1，这时需要建立一个新的连接，具体来说也就是初始化一个新的`socket`和`Transmission Control Block (TCB)`，然后将其作为`socket pair`的值插入`HashMap`中。`TCB`中的数据会被用在保证TCP可靠性的各种计算中，具体参考`RFC 793 page 19`。注意此时我们的状态为`SYN-RECEIVED`。

2. 接下来需要回应对方，表示这个接收到了这个连接请求。 即我们需要发送一个`tcp
packet`，此时需要构建合适的`tcp
header`。这实际上属于二次握手的内容，但由于在这一步方法是一次性使用的，那么在这里同时设置好这个`header`就可以避免在其它方法中做额外的工作。这个header需要设置`SYN`和`ACK`，以及我们的窗口信息、序列号等。
    然后发送这个`SYN ACK`的`tcp
packet`，发起第二次握手。

    为了发起这次握手，我们首先需要对自己的TCB做出合理的修改，然后要设置一个合理的`TCP/IP
    header`交给nic。这两个步骤可以用一个方法`write`方法来完成。为了避免冗余，这个方法需要在其它阶段也可以使用。

    tcp header的内容需要根据tcp header format(RFC793 page
15)来设置，单实际上很多内容已经被包含在了这个连接的TCB中，可以直接使用，例如端口号，序列号，窗口大小等。一定要不能忘记`checksum`需要单独计算。

3. 在发起二次握手后，等待对方回应`ACK`了（对方发起第三次握手）。接下来就可以进入`ESTABLISHED`状态啦。

以上是被动打开的情况，我们这里没有完成主动打开。所以也就没有实现同时发起连接，不过在我们的实现中是可以正常处理同时发起连接时的数据包的，只是这种情况不会发生:wink:。


4. 建立连接的时候需要注意一些问题
   * `passive
     OPEN`：当我们解析一个`packet`时，发现它拥有未知的地址和端口（`unspecified
     foreign socket`），此时我们就需要创建一个新的`TCB`，`TCB`状态设置为`Listen`。
     注意可能同时有很多这样的新`TCB`，所以需要一个`pending`的数据结构来存储这些`TCBs`，然后交给另外的线程处理`TCBs`。`Closed`同理。不过我们这里在接收到一个新`packet`后，直接处理而不是放入等待队列，所以不用设置`Listen`状态，因为这个接下来，要么将包对应的`TCB`标记为`SYN-RECV`，要么直接删除`TCB`。`Closed`同理，连接4次挥手之后（`TMIEWAIT`也结束），如果直接删除`TCB`，所以也不用设置`Closed`状态。
     * `Listen`状态是有意义的。在`dup SYN`和`Half-Open`的情况下，连接在接收到`RST`后，`TCB`状态会变为`Listen`而不是被删除，并在接下来会再次握手来恢复连接。
   * 另外，握手阶段的`tcp packet`只有`header`没有`data`

    * 在发起第二次握手的`write`方法完成之后，连接的状态依旧是`SYN-RECEIVED`，需要等待对方回应之后才能转变为`ESTABLISHED`，但是在这之前是没有超时重传的，这里的`TIMEOUT`会交给应用层。

    * 我们发送的数据长度不能超过MTU，所以数据可能需要被分成多个`segment`发送。每个`segment`是一个单独的`tcp
      packet`，它们在传输的过程中可能丢失也可能乱序抵达目标，所以对于发送数据这一过程来说，我们可能暂时没有收到对已经发送了的数据包的`ack`信息。那么我们需要对接来要发送的数据包设置正确的`sequence
      number`。同时由于`unack
      packets`的存在和对方接收窗口的限制，我们所允许发送的数据长度也需要调整，`tcp
      header`中的`sequence number`的设置需要参考RFC793 page 19的关于`Send
      Sequence Space`的定义。
    * 由于tcp header中sequence
    number只有32bits，所以需要对数值溢出后进行处理，不过rust数值类型有一个很方便的wrapping_add()的方法。

    * 注意第三次握手时的`ACK`数据包，也就是`ESTABLISHED`之前这次握手。这个数据包是可以携带数据的，甚至可以是`FIN`，所以从这一步开始就要处理数据了。
    * `TCP`协议认为`FIN`的长度是`1`，所以如果接受到一个`FIN`数据包，即使`payload`长度为`0`，也要修改`TCB`中序列号相关的字段。



### 通信过程

#### segment arrives
建议参考`RFC page
65`的说明，然后看代码，挺好理解的，只是内容和细节比较多。
这里主要解释一下`sequence number`的范围的处理。

`SEG.SEQ` 表示接收到的包的`sequence number`，那么我们可以计算出这个数据包最后一个`sequence number`的值应该是`SEG.SEQ+SEG.LEN-1`。这两个`sequence number`的范围应该满足如下两个条件之一：

```
1. RCV.NXT =< SEG.SEQ < RCV.NXT+RCV.WND
2. RCV.NXT =< SEG.SEQ+SEG.LEN-1 < RCV.NXT+RCV.WND
```

这两个条件分别测试数据包起始序列号和结束序列号是否处于接收窗口范围内，满足任意一个即可（这是因为假设了连接双方都会遵守`TCP`协议，那么只要满足一个，根据协议可以认为另一个条件自然也会满足）。

这里需要注意`tcp header`的`sequence number`是长度为`32`位的二进制数，那么范围在`[0, 2**32 -1]`，为了表示出无限增长的序列号，`tcp header`中相应的字段会在这个范围内循环。所以在实现`tcp`协议的时候，要处理`sequence number + data.len > 2**32 - 1`的情况，即数据包的起始序列号加上数据包长度超过了上限。为了处理这种情况，`RFC 793`要求对于序列号的计算都要用序列号对`2 ** 32`取模后再使用，不过在`rust`中用`wrapping_add`这个方法处理这种情况。


#### send call
我们实现一个`write`方法来实现发送数据，这在前面有过说明，但是注意在通信阶段需要为长度不为0的数据包设置一个定时器。

#### timer
我们设置一个`on_tick`方法来完成从发送队列读取数据然后调用`protocol::write`的工作，同时需要检查数据包的超时重传。步骤如下：

1. `protocol::write`将`tcp
   packet`发送到网卡的时候，使用一个`BtreeMap`来保存这个`packet`的序列号和发送时间，并将这个`tcp`连接的`una`设置为这个序列号。
2. `timer`从`unacked`队列拿数据时，会先检测`una`这个序列号对应的`packet`是否已经超时（当前时间与`BtreeMap`中保存的时间之差大于`TIMEOUT`）。显然超时的话就重新发送这个包，此时这个包的发送时间会被`protocol::write`重置。 如果没有超时的`packet`，则发送`send.nxt`。发送的工作有`protocol::write`完成。此外`srtt`的更新也是在这里完成的。

#### close
在`TCB`中需要设置两个字段来完成连接的正确关闭，用`closed`表示连接状态已经改变，我们需要发送`FIN`。一个是`closed_at`，我们已经发送了`FIN`。这样做是因为状态改变和发送数据发生在不同过程中，中间会释放锁。

#### retransmit
我们这里的实现类似于`停-等`方式，即假如我们没有收到上一次发送的`segment`的`ACK`消息，那么我们就不会发送新的数据，所以我们需要重传的数据只有一段。注意这里的数据指`tcp
packet`中的`payload`长度大于0。我们依旧可以正常`ACK`对方发送过来的`segment`，我们的`ack
number`需要根据实际情况变化，但是`sequence
unmber`和`send.nxt`不会变化，对方的接收窗口即我们的发送窗口`send.wnd`由于没有收到数据所以也不会变化。我们这里重传就很简单，只需要根据`seq
number`和`send.nxt`从我们的`unacked`队列中重新发送相应的数据就可以了。

## 接口设计

我们也要简单处理一下多线程的问题`C10k`。`C10k`的解决方法有很多，例如多线程、`同步/异步`，`阻塞/非阻塞`，`IO复用`等。我们这里使用`io复用`的方式来处理多线程。


### 网卡接口
网卡`nic`负责数据的收发，需要使用一个无限循环来保证持续监听数据。

1. 收发和发送数据。这是接口的最基本功能，我们这里和网卡通信的数据包协议是`Ipv4`协议，暂时不处理`Ipv6`数据包。这时就要使用一个额外的线程完成这个工作，也就是图1中的`packet
   loop`。这个线程在初始化网卡时同时被创建。
2. 提供一个锁，也就是`Arc`。我们认为网卡在任何时刻只能被一个连接使用。

所以根据以上考虑，网卡接口的数据结构由二元组构成，分别是`Joinhandle`和`Arc<...>`。

`tcp`连接需要指定端口，所以需要实现一个类似`socket`编程中的`bind`方法，用于为`TcpListener`指定端口。因此，我们设计的网卡接口至少需要实现三个方法：
1. `new`方法或者`default Trait`，用于初始化网卡接口。
2. `bind`方法，为`tcp`应用绑定一个端口。这个方法应该返回一个`TcpListener`结构体。
3. `Drop Trait`， 则负责程序终止时的清理

此外，连接管理器`ConnectionManager(CM)`这个结构体也在这里被创建。`CM`由两个结构体组成，`connection`和`socket
pair`。`connection`对应的是`TCP`协议中的连接部分，`pending`中的`socket
pair`则是交给`Listener`来创建`Stream`，`Stream`更接近于应用层的概念，`connection`和`Stream`是一一对应的关系。当`Stream`需要读取或者写入的时候，需要`connection`的方法来实现。

### TcpListener
在我们的设计中，`nic`负责在数据到来后唤醒`Stream`的`read`方法或者创建新的连接。

创建新连接的工作由`TcpListener`的`accept`方法来实现。

如果`packet_loop`发现数据包不存在于当前`CM`中，需要创建新的`connection`。如果创建成功，则将这个`connection`的`socket`放到`CM`的`pending`结构体末尾，并唤醒`TcpListener`来从`pending`读取连接信息创建新的`Stream`。

`ConnectionManager`通过一个`HashMap`管理着所有`connection`，`connection`中保存着`TCB`信息。另外还需要通过一个`HashMap`保存未被创建为`Stream`的`SocketPair`。


### Read

我们的`read`逻辑是，当`incoming`为空时，一直阻塞`read`线程直到被唤醒；当`incoming`不为空时，循环读出直到`incoming`为空。
`main`需要一直循环创建`stream::read`直到连接关闭，不这样做的话会导致`packet_loop`中的`Action::READ`没有唤醒对象，进而阻塞。

### Write
这是一个`socket`接口，将用户数据交给`connection`的`unacked`队列，这个数据需要被缓存到队列而不是直接发送是为了超时重传。

### Shutdown
我我们调用shutdown这个函数来发送FIN。将`TCB`中的`closed`设置为true。
注意当我们在`ESTABLISH`的状态接受到`FIN`处于`CLOSE
WAIT`状态，此时仍然可以正常发送数据。只有当我们调用`close()`后，我们才发送`FIN`进入`LASTACK`。


## 其它笔记

#### header中序列号的循环计数问题

解决方案来自`RFC 1323`

> From RFC1323:
>     TCP determines if a data segment is "old" or "new" by testing whether its sequence number is within 2^31 bytes of the left edge of the window, and if it is not, discarding the data as "old".  To insure that new data is never mistakenly considered old and vice-versa, the left edge of the sender's window has to be at most 2^31 away from the right edge of the receiver's window.

![](https://s2.loli.net/2021/12/20/iypX8xn3TAWvFDm.png)

`rust`对应的代码如下：

```
fn within_window(lhs:u32, rhs:u32) -> bool {
    lhs.wrapping_sub(rhs) > (1 << 31)
}
```

 注意这个`1 << 31`的原因。`RFC 1323`为了实现窗口扩大选项（`windows scale option`）和拥塞控制`Congestion Control`以及对应的计算，在`TCB`中使用了`32 bits`的变量来保存窗口大小（包括发送、接收和拥塞窗口）。在序列号的循环计数时，为了保证新旧`header`中序列号不被混淆，要求发送者窗口左边界（`ackn`）与接收者窗口的有边界（`rcv.nxt + rcv.wnd`）的距离不能超`2^31`。

RFC 793中的建议是将所有参与计算的值`mod 2**32`，但这在实际编码的时候还是需要做一堆比较。所以为了方便，我们还是用RFC1323这个标准好了....

#### 乱序重组
tcp packet在传输过程中可能丢包，超时重传机制只保证在丢包时重新发送，并不能保证tcp packet按顺序抵达。不过我们这里并没有实现SACK等，所以不用考虑乱序重组。
RFC 793中在接收时，只要求segment在接收窗口内就可以了，在给出的例子中，假设了接收到的segment number总是会等于RCV.NXT，也就不会乱序。对于segment number大于RCV.NXT的情况（也就是说这个包“提前”到了），只是说了Segments with higher begining sequence numbers may be held for later processing.

#### Tips

- `ip header` 中的`ihl`指的是`ip header`的长度，但是单位是`32bits`，所以使用的时候一般要乘以`4`来得到`bytes`或者`32`来得到`bits`。`ihl`最小值是`5`，最大值是`15`，也就是说`ip header`的最短长度为`20bytes`或者`160bits`（此时`option`字段为空），最大长度为`60bytes`或者`480bits`。

- 当我们用`nc`测试发送字符的时候，回车键`LF`字符也会被发送出去

- TUN frame format:
```
flags -> 2 bytes (IFF_TUN, IFF_TAP, IFF_NO_PI, basically tun device info)
proto -> 2 bytes (frame type, like IP, IPv6, ARP..., keyword: ether type)
raw protocol frame (IP package, etc. 46~1500 bytes, MTU = 1500 bytes and here is used for IP package in network layer, MTU is not fixed and is set by linux up to 65535 )
MTU in link layer may larger than 1500 due to CRC.
```

- 大小端
```
network -> big endian -> u16::from_be_ending
x86_pc -> little endian
```

- Ethernet MAC frame
```
target MAC -- source MAC -- type(ARP/IP/...) -- data -- FCS(校验)
```

- data type
```
in the ip stream is octect of [u8] in rust, which should use vec or slice.
data type of the port number and window size are u16, see TCP Header Format
data type of the seq/ack number, nxt, una... are u32, see TCP Header Format
data type of the length of the data are usize
```

- ctrl-c cannot break loop:

    bash implements WCE(wait and cooperative exit) for SIGINT and SIGQUIT
bash will wait the process exists and then exit bash itself.
bash will exit only if the current running process dies of SIGINI or SIGQUIT.

    solution: use `trap`
